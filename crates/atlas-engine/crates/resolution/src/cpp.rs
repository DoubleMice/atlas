//! Conservative C++ call lookup from written qualification and visible declarations.
//!
//! Object types, ADL, using directives, templates and overload conversions are not
//! inferred here. Unsupported calls stay unresolved; they must never fall through
//! to project-wide simple-name/proximity selection.

use std::collections::{BTreeMap, HashSet};

use crate::context::ResolutionContext;
use types::*;

pub(crate) fn resolve_call(
    reference: &ReferenceUse,
    ctx: &ResolutionContext,
    candidates: &[SymbolDef],
    imported_files: &HashSet<FileId>,
) -> Option<ResolvedTarget> {
    let arity = reference.arity? as usize;
    // A local/parameter binding can hide a function (e.g. function pointers).
    if reference.binding_id.is_some() {
        return None;
    }
    let owner = reference
        .source_symbol
        .and_then(|id| ctx.symbols_by_id.get(&id))
        .and_then(|symbol| {
            symbol
                .qualified_name
                .rsplit_once("::")
                .map(|(owner, _)| owner)
        })
        .unwrap_or("");
    let member_call = reference
        .receiver
        .as_deref()
        .is_some_and(|receiver| reference.text != format!("{receiver}::{}", reference.name));
    if member_call && reference.receiver.as_deref() != Some("this") {
        // Receiver expression is available, but its type is not yet modeled.
        return None;
    }
    let written = if member_call {
        if owner.is_empty() {
            return None;
        }
        reference.name.clone()
    } else if let Some(receiver) = &reference.receiver {
        format!("{receiver}::{}", reference.name)
    } else {
        reference.text.clone()
    };
    if !written
        .trim_start_matches("::")
        .split("::")
        .all(plain_identifier)
    {
        return None;
    }

    let absolute = written.starts_with("::");
    let mut scope = if absolute { "" } else { owner };
    loop {
        let qname = if scope.is_empty() {
            written.trim_start_matches("::").to_string()
        } else {
            format!("{scope}::{written}")
        };
        let visible: Vec<_> = candidates.iter().filter(|s| {
            s.language == Language::Cpp && s.qualified_name == qname
                && (s.file_id == reference.file_id || imported_files.contains(&s.file_id))
                // A later free-function definition is not a visible declaration.
                && (s.file_id != reference.file_id || s.name_range.start_byte <= reference.range.start_byte
                    || (s.kind == SymbolKind::Method && (member_call || reference.receiver.is_none())
                        && s.qualified_name.rsplit_once("::").is_some_and(|(parent, _)| parent == owner)))
        }).collect();
        if !visible.is_empty() {
            return choose_target(reference, &qname, &visible, candidates, arity);
        }
        // Without base-class lookup, a missing member must not become a
        // namespace-level same-name function. Bare calls stay in their owner;
        // lookup across enclosing namespaces needs additional scope evidence.
        if member_call || reference.receiver.is_none() || absolute || scope.is_empty() {
            return None;
        }
        scope = scope
            .rsplit_once("::")
            .map(|(parent, _)| parent)
            .unwrap_or("");
    }
}

fn plain_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn choose_target(
    reference: &ReferenceUse,
    qname: &str,
    visible: &[&SymbolDef],
    candidates: &[SymbolDef],
    arity: usize,
) -> Option<ResolvedTarget> {
    // A written qualifier suppresses argument-dependent lookup. Reading a
    // template type in such a parameter is not template instantiation or an
    // inferred receiver type. Unqualified calls keep the narrower subset.
    let allow_template_types = reference
        .receiver
        .as_deref()
        .is_some_and(|receiver| reference.text == format!("{receiver}::{}", reference.name));
    // Lookup terminates at the first visible name. Unknown signatures, hiding,
    // and multiple applicable overloads cannot be repaired by a wider search.
    let mut applicable: BTreeMap<String, Vec<&SymbolDef>> = BTreeMap::new();
    for symbol in visible {
        if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
            return None;
        }
        let parameters = simple_parameters(symbol.signature.as_deref()?, allow_template_types)?;
        if parameters.minimum <= arity && arity <= parameters.maximum {
            applicable
                .entry(parameters.identity)
                .or_default()
                .push(symbol);
        }
    }
    if applicable.len() != 1 {
        return None;
    }
    let (identity, declarations) = applicable.into_iter().next()?;
    // Do not select a runtime override for an implicit/this virtual call.
    let virtual_call = reference
        .receiver
        .as_deref()
        .is_none_or(|receiver| receiver == "this")
        && declarations.iter().any(|s| {
            s.signature
                .as_deref()
                .is_some_and(|s| s.split_whitespace().any(|word| word == "virtual"))
        });
    let visible_bodies: Vec<_> = declarations
        .iter()
        .copied()
        .filter(|s| s.range != s.name_range)
        .collect();
    let bodies: Vec<_> = if virtual_call {
        Vec::new()
    } else if !visible_bodies.is_empty() {
        visible_bodies
    } else {
        candidates.iter().filter(|s| {
            s.language == Language::Cpp && s.qualified_name == qname
                && matches!(s.kind, SymbolKind::Function | SymbolKind::Method)
                // C++ extraction expands definitions to the function scope;
                // file/namespace prototypes and class declarations retain the name range.
                && s.range != s.name_range
                && s.signature.as_deref().and_then(|s| simple_parameters(s, allow_template_types)).is_some_and(|p| p.identity == identity)
        }).collect()
    };
    let target = match bodies.as_slice() {
        [body] => *body,
        [] => match declarations.as_slice() {
            [declaration] => *declaration,
            _ => return None,
        },
        _ => return None,
    };
    Some(ResolvedTarget {
        symbol_id: target.id,
        confidence: Confidence::certain(),
        strategy: ResolutionStrategy::ExactMatch,
        provenance: Provenance::TreeSitter,
    })
}

struct Parameters {
    identity: String,
    minimum: usize,
    maximum: usize,
}

/// Read only the compact, plain parameter-list subset emitted by extraction.
/// Complex declarators/default expressions remain unknown, rather than being
/// assigned a possibly wrong arity. Identity retains names conservatively:
/// declarations using different parameter names need a richer type model.
fn simple_parameters(signature: &str, allow_template_types: bool) -> Option<Parameters> {
    let rest = signature.strip_prefix('(')?;
    let (params, suffix) = rest.split_once(')')?;
    if params.contains(['(', '[', ']', '{', '}', '\'', '"', '.']) {
        return None;
    }
    let params = params.trim();
    if params.is_empty() || params == "void" {
        return Some(Parameters {
            identity: format!(
                "(){suffix_qualifiers}",
                suffix_qualifiers = suffix.split(':').next().unwrap_or("").trim()
            ),
            minimum: 0,
            maximum: 0,
        });
    }
    let mut minimum = 0;
    let mut parts = Vec::new();
    let mut saw_default = false;
    for param in parameter_parts(params, allow_template_types)? {
        let (declaration, default) = param
            .split_once('=')
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((param, None));
        if let Some(default) = default {
            if !default
                .trim()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+')
            {
                return None;
            }
            saw_default = true;
        } else {
            if saw_default {
                return None;
            }
            minimum += 1;
        }
        let declaration = declaration.split_whitespace().collect::<Vec<_>>().join(" ");
        if declaration.is_empty() {
            return None;
        }
        parts.push(declaration);
    }
    Some(Parameters {
        identity: format!(
            "({}){}",
            parts.join(","),
            suffix.split(':').next().unwrap_or("").trim()
        ),
        minimum,
        maximum: parts.len(),
    })
}

/// Split only at commas outside written template type arguments. Expressions
/// containing parentheses, comparisons/defaults inside angle brackets, packs
/// and complex declarators remain unsupported; no type equality is inferred.
fn parameter_parts(params: &str, allow_template_types: bool) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth: usize = 0;
    for (offset, ch) in params.char_indices() {
        match ch {
            '<' if allow_template_types => depth += 1,
            '>' if allow_template_types => depth = depth.checked_sub(1)?,
            '<' | '>' => return None,
            '=' if depth > 0 => return None,
            ',' if depth == 0 => {
                parts.push(&params[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(&params[start..]);
    Some(parts)
}
