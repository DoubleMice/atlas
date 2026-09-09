//! Read-only, source-located investigation of stored calls.
//!
//! Lexical declarations and names written in type syntax are investigation
//! context, not points-to results or new call edges. No resolution rows are written.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
    sync::Arc,
};

use db::Store;
use tree_sitter::{Node, Parser, Tree};
use types::*;

mod cpp;

pub const MAX_CONTEXT_FILE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONTEXT_TOTAL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CONTEXT_ITEMS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLocation {
    pub file_id: FileId,
    pub range: TextRange,
}

#[derive(Debug, Clone)]
pub struct CallContextItem {
    pub call: ReferenceUse,
    /// The item's role in an investigation, never a relationship strength.
    pub role: &'static str,
    pub location: ContextLocation,
    pub symbol_id: Option<SymbolId>,
    pub related_locations: Vec<ContextLocation>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct CallContextGap {
    pub call: ReferenceUse,
    pub code: &'static str,
    pub message: String,
    pub related_locations: Vec<ContextLocation>,
}

#[derive(Debug, Default)]
pub struct CallContextResult {
    pub items: Vec<CallContextItem>,
    pub gaps: Vec<CallContextGap>,
    pub files_read: usize,
    pub bytes_read: usize,
}

struct ParsedSource {
    source: String,
    tree: Tree,
}

struct Investigation<'a> {
    store: &'a Store,
    root: &'a Path,
    canceled: &'a dyn Fn() -> bool,
    parsed: BTreeMap<FileId, Result<Arc<ParsedSource>, String>>,
    result: CallContextResult,
}

/// Inspect existing references that intersect a source byte range. This uses
/// stored lexical scopes/bindings and parses only the source needed for their
/// declaration/initializer syntax. Currently C++ supplies receiver context;
/// unsupported languages retain an explicit limitation. Results never alter Store.
pub fn inspect_call_context(
    store: &Store,
    root: &Path,
    path: &str,
    start_byte: u32,
    end_byte: u32,
    canceled: &dyn Fn() -> bool,
) -> anyhow::Result<CallContextResult> {
    let mut query = Investigation {
        store,
        root,
        canceled,
        parsed: BTreeMap::new(),
        result: CallContextResult::default(),
    };
    query.check()?;
    let Some(file) = store
        .find_files_by_path_prefix(path)?
        .into_iter()
        .find(|f| f.path == path)
    else {
        return Ok(query.result);
    };
    let references = store.find_references_by_file(&file.file_id)?;
    let calls: Vec<_> = references
        .iter()
        .filter(|r| {
            r.kind == ReferenceKind::Call
                && r.range.start_byte < end_byte
                && start_byte < r.range.end_byte
        })
        .collect();
    if calls.is_empty() {
        return Ok(query.result);
    }
    if file.language != Language::Cpp {
        for call in calls {
            query.gap(call, "syntax_context_unsupported", "Receiver context is not implemented for this language; stored calls and name candidates remain available.", vec![])?;
        }
        return Ok(query.result);
    }
    let bindings = store.find_bindings_by_file(&file.file_id)?;
    let scopes: BTreeMap<_, _> = store
        .find_scopes_by_file(&file.file_id)?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    for call in calls {
        query.check()?;
        query.receiver(call, &references, &bindings, &scopes)?;
    }
    query.check()?;
    Ok(query.result)
}

impl Investigation<'_> {
    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!(self.canceled)(), "call context canceled");
        Ok(())
    }

    fn reserve_item(&self) -> anyhow::Result<()> {
        self.check()?;
        anyhow::ensure!(
            self.result.items.len() + self.result.gaps.len() < MAX_CONTEXT_ITEMS,
            "call context exceeds 10000 records; select a smaller region"
        );
        Ok(())
    }

    fn item(
        &mut self,
        call: &ReferenceUse,
        role: &'static str,
        location: ContextLocation,
        symbol_id: Option<SymbolId>,
        related_locations: Vec<ContextLocation>,
        message: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.reserve_item()?;
        self.result.items.push(CallContextItem {
            call: call.clone(),
            role,
            location,
            symbol_id,
            related_locations,
            message: message.into(),
        });
        Ok(())
    }

    fn gap(
        &mut self,
        call: &ReferenceUse,
        code: &'static str,
        message: impl Into<String>,
        related_locations: Vec<ContextLocation>,
    ) -> anyhow::Result<()> {
        self.reserve_item()?;
        self.result.gaps.push(CallContextGap {
            call: call.clone(),
            code,
            message: message.into(),
            related_locations,
        });
        Ok(())
    }

    fn source(&mut self, id: FileId) -> Result<Arc<ParsedSource>, String> {
        if let Some(parsed) = self.parsed.get(&id) {
            return parsed.clone();
        }
        let parsed = self.load_source(id).map(Arc::new);
        self.parsed.insert(id, parsed.clone());
        parsed
    }

    fn load_source(&mut self, id: FileId) -> Result<ParsedSource, String> {
        self.check().map_err(|e| e.to_string())?;
        let file = self
            .store
            .get_file(&id)
            .map_err(|e| e.to_string())?
            .ok_or("source metadata is unavailable")?;
        let root = self.root.canonicalize().map_err(|e| e.to_string())?;
        let expected = root.join(&file.path);
        let path = expected.canonicalize().map_err(|e| e.to_string())?;
        if path != expected || !path.starts_with(&root) {
            return Err("source path is not an ordinary file inside the selected root".into());
        }
        let mut input = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let metadata = input.metadata().map_err(|e| e.to_string())?;
        let remaining = MAX_CONTEXT_TOTAL_BYTES.saturating_sub(self.result.bytes_read);
        if !metadata.is_file() || metadata.len() > MAX_CONTEXT_FILE_BYTES.min(remaining) as u64 {
            return Err("source exceeds the 8 MiB file or 32 MiB investigation read limit; read a smaller source range separately".into());
        }
        self.result.files_read += 1;
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            self.check().map_err(|e| e.to_string())?;
            let n = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            self.result.bytes_read += n;
            if bytes.len() + n > MAX_CONTEXT_FILE_BYTES
                || self.result.bytes_read > MAX_CONTEXT_TOTAL_BYTES
            {
                return Err("source grew beyond the investigation read limit".into());
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        let source = String::from_utf8(bytes).map_err(|_| "context source is not valid UTF-8")?;
        let frontend =
            extraction::create_frontend(file.language).ok_or("source grammar is unavailable")?;
        let mut parser = Parser::new();
        parser
            .set_language(&frontend.parser.tree_sitter_language())
            .map_err(|e| e.to_string())?;
        let parser_source = frontend.parser.parser_source(&source);
        if parser_source.len() != source.len() {
            return Err("parser source does not preserve byte positions".into());
        }
        let mut progress = |_: &tree_sitter::ParseState| {
            if (self.canceled)() {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        };
        let tree = parser
            .parse_with_options(
                &mut |offset, _| parser_source.as_bytes().get(offset..).unwrap_or(&[]),
                None,
                Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
            )
            .ok_or("context parse was interrupted")?;
        drop(parser_source);
        Ok(ParsedSource { source, tree })
    }

    fn receiver(
        &mut self,
        call: &ReferenceUse,
        references: &[ReferenceUse],
        bindings: &[BindingDef],
        scopes: &BTreeMap<ScopeId, ScopeDef>,
    ) -> anyhow::Result<()> {
        if call.receiver.is_none() {
            return Ok(());
        }
        let parsed = match self.source(call.file_id) {
            Ok(source) => source,
            Err(error) => return self.gap(call, "source_context_unavailable", error, vec![]),
        };
        let Some(receiver) = cpp::receiver(parsed.tree.root_node(), call.range) else {
            // Written namespace/type qualification is not an object receiver.
            if call
                .receiver
                .as_ref()
                .is_some_and(|r| call.text == format!("{r}::{}", call.name))
            {
                return Ok(());
            }
            return self.gap(
                call,
                "receiver_syntax_unavailable",
                "The stored call could not be associated with a supported member-call expression.",
                vec![],
            );
        };
        let location = loc(call.file_id, receiver);
        if receiver.has_error() {
            return self.gap(
                call,
                "receiver_syntax_unavailable",
                "The receiver expression has syntax errors; no binding is selected.",
                vec![location],
            );
        }
        if receiver.kind() == "call_expression" {
            return self.initializer(call, receiver, references, location);
        }
        let Some((name, explicit_member)) = cpp::receiver_name(receiver, &parsed.source) else {
            return self.gap(call, "receiver_expression_unsupported", "The receiver is not a simple lexical identifier or direct call; inspect its source expression.", vec![location]);
        };
        let found = if explicit_member {
            vec![]
        } else {
            visible_bindings(
                bindings,
                scopes,
                call.scope_id,
                name,
                receiver.start_byte() as u32,
            )
        };
        if found.is_empty() {
            if cpp::inside_lambda(receiver) {
                return self.gap(call, "receiver_capture_unverified", "No lexical binding accounts for the lambda receiver; captures and enclosing-object access must be checked before attributing a class field.", vec![location]);
            }
            if self.member_receiver(call, name, location.clone())? {
                return Ok(());
            }
            return self.gap(call, "receiver_binding_unavailable", "No recorded lexical binding or direct field in the caller's class accounts for this receiver. Inheritance, captures and missing declarations remain open.", vec![location]);
        }
        if found.len() > 1 {
            self.gap(call, "receiver_binding_ambiguous", "Several declarations introduce this name in the nearest recorded scope; all are retained without selecting a value.", found.iter().map(|b| ContextLocation { file_id: b.file_id, range: b.range }).collect())?;
        }
        for binding in found {
            self.check()?;
            let Some(syntax) = cpp::binding(parsed.tree.root_node(), binding.range) else {
                self.item(
                    call,
                    "receiver_binding",
                    ContextLocation {
                        file_id: binding.file_id,
                        range: binding.range,
                    },
                    binding.symbol_id,
                    vec![location.clone()],
                    "Recorded lexical binding; its declaration syntax is unavailable.",
                )?;
                self.gap(
                    call,
                    "binding_syntax_unavailable",
                    "Cannot locate the binding's declaration and initializer syntax.",
                    vec![ContextLocation {
                        file_id: binding.file_id,
                        range: binding.range,
                    }],
                )?;
                continue;
            };
            let declaration = loc(call.file_id, syntax.declaration);
            self.item(call, "receiver_binding", declaration.clone(), binding.symbol_id, vec![location.clone()], format!("Lexical declaration of {name}; an initializer is not proof of the receiver's current value."))?;
            if let Some(ty) = syntax.type_node {
                self.type_context(call, ty, &parsed, declaration.clone())?;
            }
            if let Some(value) = syntax.initializer {
                self.initializer(call, value, references, declaration)?;
            }
        }
        self.gap(call, "receiver_semantics_unverified", "Declaration/initializer/type syntax is available, but assignments, aliases, template semantics and runtime dispatch have not been established by this investigation.", vec![location])
    }

    fn member_receiver(
        &mut self,
        call: &ReferenceUse,
        name: &str,
        receiver: ContextLocation,
    ) -> anyhow::Result<bool> {
        let Some(caller) = call
            .source_symbol
            .map(|id| self.store.find_symbol_by_id(&id))
            .transpose()?
            .flatten()
        else {
            return Ok(false);
        };
        let Some((owner, _)) = caller.qualified_name.rsplit_once("::") else {
            return Ok(false);
        };
        let mut fields = vec![];
        for field in self
            .store
            .find_symbols_by_qname(&format!("{owner}::{name}"))?
        {
            self.check()?;
            if field.language != Language::Cpp || field.kind != SymbolKind::Field {
                continue;
            }
            let Some(container) = field
                .container
                .map(|id| self.store.find_symbol_by_id(&id))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            if matches!(container.kind, SymbolKind::Class | SymbolKind::Struct)
                && container.qualified_name == owner
            {
                fields.push(field);
            }
        }
        if fields.is_empty() {
            return Ok(false);
        }
        if fields.len() > 1 {
            self.gap(call, "receiver_binding_ambiguous", "Several indexed class-field declarations account for this name; all are retained without selecting a build variant or object value.", fields.iter().map(|f| ContextLocation { file_id: f.file_id, range: f.name_range }).collect())?;
        }
        for field in fields {
            let at = ContextLocation {
                file_id: field.file_id,
                range: field.name_range,
            };
            let parsed = match self.source(field.file_id) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.item(
                        call,
                        "receiver_binding",
                        at.clone(),
                        Some(field.id),
                        vec![receiver.clone()],
                        "Indexed field in the caller's class; declaration source is unavailable.",
                    )?;
                    self.gap(call, "source_context_unavailable", error, vec![at])?;
                    continue;
                }
            };
            let Some(syntax) = cpp::binding(parsed.tree.root_node(), field.name_range) else {
                self.item(
                    call,
                    "receiver_binding",
                    at.clone(),
                    Some(field.id),
                    vec![receiver.clone()],
                    "Indexed field in the caller's class; declaration syntax is unavailable.",
                )?;
                self.gap(
                    call,
                    "binding_syntax_unavailable",
                    "Cannot associate the indexed field with supported declaration syntax.",
                    vec![at],
                )?;
                continue;
            };
            let declaration = loc(field.file_id, syntax.declaration);
            self.item(call, "receiver_binding", declaration.clone(), Some(field.id), vec![receiver.clone()], "Field declared in the caller's class; this establishes a declaration origin, not the receiver's current value or smart-pointer semantics.")?;
            if let Some(ty) = syntax.type_node {
                self.type_context(call, ty, &parsed, declaration)?;
            }
        }
        self.gap(call, "receiver_semantics_unverified", "Class-field declarations and written types are available. Initializers, assignments, aliases, dereference operators and runtime dispatch remain unverified.", vec![receiver])?;
        Ok(true)
    }

    fn initializer(
        &mut self,
        call: &ReferenceUse,
        value: Node<'_>,
        references: &[ReferenceUse],
        origin: ContextLocation,
    ) -> anyhow::Result<()> {
        let location = loc(call.file_id, value);
        self.item(call, "initializer", location.clone(), None, vec![origin], "Expression written at the receiver's origin; this is not a points-to or reaching-value result.")?;
        if value.has_error() || value.kind() != "call_expression" {
            return self.gap(call, "initializer_value_unestablished", "Only a direct call initializer has callable-return context here; aliases, compound expressions and later assignments remain open.", vec![location]);
        }
        let Some(callee) = value.child_by_field_name("function") else {
            return Ok(());
        };
        let init_calls: Vec<_> = references
            .iter()
            .filter(|r| {
                r.kind == ReferenceKind::Call
                    && r.range.start_byte >= callee.start_byte() as u32
                    && r.range.end_byte <= callee.end_byte() as u32
            })
            .collect();
        if init_calls.len() != 1 {
            return self.gap(
                call,
                "initializer_call_unavailable",
                "No unique stored call reference accounts for this initializer's callee.",
                vec![location],
            );
        }
        let init_call = init_calls[0];
        let declarations = if let Some(target) = &init_call.resolved {
            match self.store.find_symbol_by_id(&target.symbol_id)? {
                Some(symbol) => self.store.find_symbols_by_qname(&symbol.qualified_name)?,
                None => vec![],
            }
        } else {
            self.gap(call, "initializer_target_unresolved", "The initializer call has no resolved target; same-name callable declarations are retained as hypotheses.", vec![location.clone()])?;
            self.store.find_symbols_by_name(&init_call.name)?
        };
        let mut found = false;
        for symbol in declarations.into_iter().filter(|s| {
            s.language == Language::Cpp
                && matches!(s.kind, SymbolKind::Function | SymbolKind::Method)
        }) {
            self.check()?;
            found = true;
            let file = match self.source(symbol.file_id) {
                Ok(file) => file,
                Err(error) => {
                    let at = ContextLocation {
                        file_id: symbol.file_id,
                        range: symbol.name_range,
                    };
                    self.item(
                        call,
                        "callable_declaration",
                        at.clone(),
                        Some(symbol.id),
                        vec![location.clone()],
                        "Indexed callable candidate; its source could not be inspected.",
                    )?;
                    self.gap(call, "source_context_unavailable", error, vec![at])?;
                    continue;
                }
            };
            let Some(declaration) = cpp::callable(file.tree.root_node(), symbol.name_range) else {
                self.gap(call, "return_declaration_unavailable", "The indexed callable has no supported declaration syntax at its recorded name.", vec![ContextLocation { file_id: symbol.file_id, range: symbol.name_range }])?;
                continue;
            };
            let at = ContextLocation {
                file_id: symbol.file_id,
                range: declaration.header,
            };
            let basis = if init_call.resolved.is_some() {
                "Declaration with the indexed initializer target's qualified name; each overload remains an investigation candidate."
            } else {
                "Same-name declaration for an unresolved initializer call; applicability is unverified."
            };
            self.item(
                call,
                "callable_declaration",
                at.clone(),
                Some(symbol.id),
                vec![location.clone()],
                basis,
            )?;
            if let Some(ty) = declaration.type_node {
                self.type_context(call, ty, &file, at)?;
            } else {
                self.gap(
                    call,
                    "return_type_unavailable",
                    "The declaration does not supply a supported explicit return-type syntax.",
                    vec![at],
                )?;
            }
        }
        if !found {
            self.gap(call, "initializer_declaration_unavailable", "No indexed callable declaration accounts for the initializer; source search remains necessary.", vec![location])?;
        }
        Ok(())
    }

    fn type_context(
        &mut self,
        call: &ReferenceUse,
        ty: Node<'_>,
        parsed: &ParsedSource,
        declaration: ContextLocation,
    ) -> anyhow::Result<()> {
        for name_node in cpp::type_names(ty) {
            self.check()?;
            let name = cpp::text(name_node, &parsed.source);
            let mention = loc(declaration.file_id, name_node);
            self.item(call, "type_reference", mention.clone(), None, vec![declaration.clone()], format!("{name} is written in declaration type syntax. A template argument or alias name is not the effective receiver type."))?;
            let symbols = self.store.find_symbols_by_name(name)?;
            let mut found = false;
            for symbol in symbols.into_iter().filter(|s| {
                s.language == Language::Cpp
                    && matches!(
                        s.kind,
                        SymbolKind::Class
                            | SymbolKind::Struct
                            | SymbolKind::Enum
                            | SymbolKind::TypeAlias
                    )
            }) {
                self.check()?;
                found = true;
                let at = ContextLocation {
                    file_id: symbol.file_id,
                    range: symbol.name_range,
                };
                self.item(call, "type_declaration", at.clone(), Some(symbol.id), vec![mention.clone()], "Indexed type declaration with this written name; qualification, aliases and applicability remain to be checked.")?;
                for member in self
                    .store
                    .find_symbols_by_qname(&format!("{}::{}", symbol.qualified_name, call.name))?
                {
                    self.check()?;
                    if member.language != Language::Cpp {
                        continue;
                    }
                    self.item(call, "type_member", ContextLocation { file_id: member.file_id, range: member.range }, Some(member.id), vec![at.clone()], "Member name under a type mentioned by the receiver declaration/initializer. This is an investigation candidate, not a resolved member lookup.")?;
                }
            }
            if !found {
                self.gap(call, "type_declaration_unavailable", format!("No indexed type declaration matches the written name {name}; dependencies, aliases or extraction may be missing."), vec![mention])?;
            }
        }
        Ok(())
    }
}

fn visible_bindings<'a>(
    bindings: &'a [BindingDef],
    scopes: &BTreeMap<ScopeId, ScopeDef>,
    mut scope: Option<ScopeId>,
    name: &str,
    before: u32,
) -> Vec<&'a BindingDef> {
    let mut visited = BTreeSet::new();
    while let Some(id) = scope {
        if !visited.insert(id) {
            break;
        }
        let found: Vec<_> = bindings
            .iter()
            .filter(|b| b.scope_id == id && b.name == name && b.visible_from_byte <= before)
            .collect();
        if !found.is_empty() {
            return found;
        }
        let Some(current) = scopes.get(&id) else {
            break;
        };
        if current.kind == ScopeKind::Function {
            break;
        }
        scope = current.parent_id;
    }
    vec![]
}

fn loc(file_id: FileId, node: Node<'_>) -> ContextLocation {
    ContextLocation {
        file_id,
        range: cpp::range(node),
    }
}

#[cfg(all(test, feature = "cpp"))]
mod tests;
