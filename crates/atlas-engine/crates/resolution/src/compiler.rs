//! Associate applicable compiler-selected direct calls with existing index identities.
//!
//! The producer is responsible for compiler validity, configuration and source
//! consistency. This step checks indexed input identities and exact locations;
//! it is not a compiler or a validator of arbitrary third-party claims. It does
//! not mutate references or graphs. A building pipeline may combine its results
//! with source resolutions before publishing; Ready queries must stay read-only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path};

use db::Store;
use serde::{Deserialize, Serialize};
use types::{
    CallsiteId, Confidence, FileId, Provenance, ReferenceId, ReferenceKind, ReferenceUse,
    ResolutionStrategy, ResolvedTarget, SymbolDef, SymbolId, SymbolKind,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompilerLocation {
    pub path: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompilerDeclaration {
    pub name: String,
    pub location: CompilerLocation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerObservationKind {
    CallDeclaration,
    FieldReference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerDispatch {
    Direct,
    Virtual,
    NotACall,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompilerObservation {
    pub kind: CompilerObservationKind,
    pub location: CompilerLocation,
    /// Complete written invocation, distinct from the callee's final name token.
    pub call_expression: Option<CompilerLocation>,
    pub declaration: CompilerDeclaration,
    pub definition: Option<CompilerDeclaration>,
    pub owner: Option<CompilerDeclaration>,
    pub dispatch: CompilerDispatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerBindingGapReason {
    DynamicDispatch,
    OwnerUnavailable,
    DefinitionUnavailable,
    CallExpressionUnavailable,
    InputIdentityUnavailable,
    IndexedInputMismatch,
    InvalidLocation,
    SymbolNotUnique,
    CallsiteNotUnique,
    CallerMismatch,
    ReferenceMismatch,
    ConflictingTargets,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompilerBindingGap {
    pub location: CompilerLocation,
    pub reason: CompilerBindingGapReason,
    /// Present only when the compiler observation matches an indexed invocation.
    pub reference_id: Option<ReferenceId>,
    /// A supplied indexed dependency whose identity prevented using the input.
    pub input_path: Option<String>,
    /// Source location supplied for a virtual call's selected declaration on
    /// matching inputs. Navigation does not require an extracted symbol or a
    /// matching invocation; neither is manufactured from this position.
    pub declaration_location: Option<CompilerLocation>,
    /// An indexed declaration selected by this virtual-call observation, when
    /// its invocation and declaration locations could be associated precisely.
    /// This is a navigation endpoint, not a runtime implementation. Different
    /// observations may retain different declarations without a unique binding.
    pub declaration_id: Option<SymbolId>,
}

/// Explicit, applicable compiler observations for one building index. The caller
/// owns compiler configuration and dependency validity; `inputs` identifies the
/// indexed source bytes used by that computation, not arbitrary later files.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompilerCallInput {
    pub inputs: BTreeMap<String, String>,
    pub observations: Vec<CompilerObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompilerCallResolution {
    pub reference: ReferenceUse,
    pub callsite_id: CallsiteId,
    pub target: ResolvedTarget,
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub struct CompilerCallBindings {
    pub resolved: Vec<CompilerCallResolution>,
    pub gaps: Vec<CompilerBindingGap>,
}

struct Association<'a> {
    store: &'a Store,
    inputs: &'a BTreeMap<String, String>,
    symbols: HashMap<FileId, Vec<SymbolDef>>,
}

type Attempt<T> = anyhow::Result<Result<T, CompilerBindingGapReason>>;

impl Association<'_> {
    fn file(&self, at: &CompilerLocation) -> Attempt<FileId> {
        if at.start_byte >= at.end_byte {
            return Ok(Err(CompilerBindingGapReason::InvalidLocation));
        }
        self.indexed_file(&at.path)
    }

    fn indexed_file(&self, path: &str) -> Attempt<FileId> {
        if path.is_empty()
            || !Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Ok(Err(CompilerBindingGapReason::InvalidLocation));
        }
        let Some(expected) = self.inputs.get(path).filter(|value| !value.is_empty()) else {
            return Ok(Err(CompilerBindingGapReason::InputIdentityUnavailable));
        };
        let id = FileId::generate(path);
        let Some(file) = self.store.get_file(&id)? else {
            return Ok(Err(CompilerBindingGapReason::IndexedInputMismatch));
        };
        if file.path != path || &file.content_hash != expected {
            return Ok(Err(CompilerBindingGapReason::IndexedInputMismatch));
        }
        Ok(Ok(id))
    }

    fn symbol(&mut self, declaration: &CompilerDeclaration) -> Attempt<SymbolId> {
        let file = match self.file(&declaration.location)? {
            Ok(file) => file,
            Err(reason) => return Ok(Err(reason)),
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = self.symbols.entry(file) {
            entry.insert(self.store.find_symbols_by_file(&file)?);
        }
        let matches: Vec<_> = self.symbols[&file]
            .iter()
            .filter(|symbol| {
                matches!(
                    symbol.kind,
                    SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor
                ) && symbol.name == declaration.name
                    && symbol.name_range.start_byte == declaration.location.start_byte
                    && symbol.name_range.end_byte == declaration.location.end_byte
            })
            .collect();
        Ok(if matches.len() == 1 {
            Ok(matches[0].id)
        } else {
            Err(CompilerBindingGapReason::SymbolNotUnique)
        })
    }

    fn reference(&self, observation: &CompilerObservation) -> Attempt<(CallsiteId, ReferenceUse)> {
        use CompilerBindingGapReason as Gap;
        let Some(expression) = &observation.call_expression else {
            return Ok(Err(Gap::CallExpressionUnavailable));
        };
        if expression.path != observation.location.path
            || expression.start_byte > observation.location.start_byte
            || expression.end_byte < observation.location.end_byte
        {
            return Ok(Err(Gap::InvalidLocation));
        }
        let file = match self.file(&observation.location)? {
            Ok(id) => id,
            Err(reason) => return Ok(Err(reason)),
        };
        let calls: Vec<_> = self
            .store
            .find_callsites_by_file(&file)?
            .into_iter()
            .filter(|call| {
                call.range.start_byte == expression.start_byte
                    && call.range.end_byte == expression.end_byte
                    && call.callee_range.as_ref().is_some_and(|range| {
                        range.start_byte <= observation.location.start_byte
                            && range.end_byte >= observation.location.end_byte
                    })
            })
            .collect();
        if calls.len() != 1 {
            return Ok(Err(Gap::CallsiteNotUnique));
        }
        let call = &calls[0];
        let Some(reference_id) = call.reference_id else {
            return Ok(Err(Gap::ReferenceMismatch));
        };
        let Some(reference) = self.store.get_reference_by_id(reference_id.as_bytes())? else {
            return Ok(Err(Gap::ReferenceMismatch));
        };
        if reference.kind != ReferenceKind::Call
            || reference.file_id != file
            || reference.source_symbol != Some(call.caller)
            || Some(reference.range) != call.callee_range
        {
            return Ok(Err(Gap::ReferenceMismatch));
        }
        Ok(Ok((call.id, reference)))
    }

    fn call(
        &mut self,
        observation: &CompilerObservation,
        callsite_id: CallsiteId,
        reference: ReferenceUse,
    ) -> Attempt<CompilerCallResolution> {
        use CompilerBindingGapReason as Gap;
        if observation.dispatch != CompilerDispatch::Direct {
            return Ok(Err(Gap::DynamicDispatch));
        }
        let Some(owner) = &observation.owner else {
            return Ok(Err(Gap::OwnerUnavailable));
        };
        // A direct call selects a declaration even when its definition is not
        // available in this translation unit. Preserve that endpoint without
        // guessing another definition from a matching name or signature.
        let target = observation
            .definition
            .as_ref()
            .unwrap_or(&observation.declaration);
        let owner_id = match self.symbol(owner)? {
            Ok(id) => id,
            Err(reason) => return Ok(Err(reason)),
        };
        if reference.source_symbol != Some(owner_id) {
            return Ok(Err(Gap::CallerMismatch));
        }
        let target_id = match self.symbol(target)? {
            Ok(id) => id,
            Err(reason) => return Ok(Err(reason)),
        };
        if target.name != observation.declaration.name {
            return Ok(Err(Gap::ReferenceMismatch));
        }
        Ok(Ok(CompilerCallResolution {
            reference,
            callsite_id,
            target: ResolvedTarget {
                symbol_id: target_id,
                confidence: Confidence::new(1.0),
                strategy: ResolutionStrategy::Compiler,
                provenance: Provenance::Compiler,
            },
        }))
    }
}

/// Match compiler-selected direct calls against the same indexed source inputs.
/// `inputs` uses the index's existing per-file content identities, captured for
/// the compiler run. Callers must not populate it from an unrelated later index.
/// Missing definitions retain the selected indexed declaration and a body gap.
/// Fields, virtual dispatch, absent owners and conflicting targets cannot turn
/// into direct calls. No nearest-name or nearest-function fallback.
pub fn associate_compiler_calls(
    store: &Store,
    observations: &[CompilerObservation],
    inputs: &BTreeMap<String, String>,
    cancelled: &mut dyn FnMut() -> bool,
) -> anyhow::Result<CompilerCallBindings> {
    let mut association = Association {
        store,
        inputs,
        symbols: HashMap::new(),
    };
    let mut report = CompilerCallBindings::default();
    // A changed indexed dependency can invalidate calls in an unchanged file.
    // Validate the complete supplied indexed input set, not only edge endpoints.
    for path in inputs.keys() {
        anyhow::ensure!(!cancelled(), "compiler call association cancelled");
        if let Err(reason) = association.indexed_file(path)? {
            for observation in observations {
                anyhow::ensure!(!cancelled(), "compiler call association cancelled");
                if observation.kind == CompilerObservationKind::CallDeclaration {
                    report.gaps.push(CompilerBindingGap {
                        location: observation.location.clone(),
                        reason,
                        reference_id: None,
                        input_path: Some(path.clone()),
                        declaration_location: None,
                        declaration_id: None,
                    });
                }
            }
            return Ok(report);
        }
    }
    let mut selected: HashMap<ReferenceId, (CompilerCallResolution, CompilerLocation)> =
        HashMap::new();
    let mut conflicts = HashSet::new();
    let mut blocked_locations = HashSet::new();
    for observation in observations {
        anyhow::ensure!(!cancelled(), "compiler call association cancelled");
        if observation.kind != CompilerObservationKind::CallDeclaration {
            continue;
        }
        let location_key = observation.location.clone();
        let declaration_location = if observation.dispatch == CompilerDispatch::Virtual
            && association.file(&observation.declaration.location)?.is_ok()
        {
            Some(observation.declaration.location.clone())
        } else {
            None
        };
        let (reference_id, result, declaration_id) = match association.reference(observation)? {
            Ok((id, reference)) => {
                // The selected declaration can be useful even when the runtime
                // override or compiler caller is unavailable. Do not borrow a
                // supplied definition as that runtime target, or infer a new
                // callsite from a matching name.
                let declaration_id = if observation.dispatch == CompilerDispatch::Virtual {
                    association.symbol(&observation.declaration)?.ok()
                } else {
                    None
                };
                (
                    Some(reference.id),
                    association.call(observation, id, reference)?,
                    declaration_id,
                )
            }
            Err(reason) => (None, Err(reason), None),
        };
        match result {
            Err(reason) => {
                blocked_locations.insert(location_key);
                report.gaps.push(CompilerBindingGap {
                    location: observation.location.clone(),
                    reason,
                    reference_id,
                    input_path: None,
                    declaration_location,
                    declaration_id,
                });
            }
            Ok(binding) => {
                let id = binding.reference.id;
                if observation.definition.is_none() {
                    report.gaps.push(CompilerBindingGap {
                        location: observation.location.clone(),
                        reason: CompilerBindingGapReason::DefinitionUnavailable,
                        reference_id: Some(id),
                        input_path: None,
                        declaration_location: None,
                        declaration_id: None,
                    });
                }
                if selected
                    .get(&id)
                    .is_some_and(|(old, _)| old.target.symbol_id != binding.target.symbol_id)
                {
                    conflicts.insert(id);
                    report.gaps.push(CompilerBindingGap {
                        location: observation.location.clone(),
                        reason: CompilerBindingGapReason::ConflictingTargets,
                        reference_id: Some(id),
                        input_path: None,
                        declaration_location: None,
                        declaration_id: None,
                    });
                }
                selected.insert(id, (binding, location_key));
            }
        }
    }
    report.resolved = selected
        .into_iter()
        .filter(|(id, (_, location))| {
            !conflicts.contains(id) && !blocked_locations.contains(location)
        })
        .map(|(_, (value, _))| value)
        .collect();
    report.resolved.sort_by_key(|binding| binding.reference.id);
    Ok(report)
}

/// Replace primary source resolutions before GraphBuilder runs. A compiler
/// observation with an exact invocation identity also prevents weaker source
/// selection from hiding observed dynamic dispatch, conflicting targets or a
/// different caller. Missing compiler owners/definitions are producer limits;
/// they do not invalidate independently established source facts. Supplemental implicit
/// operator steps remain separate facts and do not resolve the written call.
/// This mutates a building Store only; existing edges must have been invalidated.
pub fn apply_compiler_call_bindings(
    store: &Store,
    source: &mut Vec<(ReferenceUse, ResolvedTarget)>,
    bindings: &CompilerCallBindings,
) -> anyhow::Result<()> {
    // A producer's missing definition is not evidence against a body already
    // selected from source. Retain that result only when its declaration identity
    // agrees with the compiler selection under the existing C++ association rules.
    // Keep source provenance: the compiler did not establish this body.
    let selected: HashMap<_, _> = bindings
        .resolved
        .iter()
        .map(|binding| (binding.reference.id, binding))
        .collect();
    let mut types = None;
    let mut preserved = HashMap::new();
    for (reference, target) in source.iter() {
        let Some(binding) = selected.get(&reference.id) else {
            continue;
        };
        if target.strategy != ResolutionStrategy::ExactMatch
            || target.provenance != Provenance::TreeSitter
            || target.symbol_id == binding.target.symbol_id
        {
            continue;
        }
        let (Some(declaration), Some(body)) = (
            store.find_symbol_by_id(&binding.target.symbol_id)?,
            store.find_symbol_by_id(&target.symbol_id)?,
        ) else {
            continue;
        };
        if declaration.range != declaration.name_range || body.range == body.name_range {
            continue;
        }
        if types.is_none() {
            types = Some(crate::cpp::TypeIndex::build(store)?);
        }
        if types
            .as_ref()
            .is_some_and(|types| types.matches_selected_declaration(&declaration, &body))
        {
            preserved.insert(reference.id, target.clone());
        }
    }
    let affected: HashSet<_> = bindings
        .resolved
        .iter()
        .map(|binding| binding.reference.id)
        .chain(
            bindings
                .gaps
                .iter()
                .filter(|gap| {
                    matches!(
                        gap.reason,
                        CompilerBindingGapReason::DynamicDispatch
                            | CompilerBindingGapReason::ConflictingTargets
                            | CompilerBindingGapReason::CallerMismatch
                    )
                })
                .filter_map(|gap| gap.reference_id),
        )
        .collect();
    source.retain(|(reference, target)| {
        !affected.contains(&reference.id) || target.strategy == ResolutionStrategy::ImplicitOperator
    });
    store.invalidate_references(&affected.into_iter().collect::<Vec<_>>())?;
    let updates: Vec<_> = bindings
        .resolved
        .iter()
        .map(|binding| {
            (
                binding.reference.id,
                preserved
                    .get(&binding.reference.id)
                    .unwrap_or(&binding.target)
                    .clone(),
            )
        })
        .collect();
    store.batch_update_resolutions(&updates)?;
    source.extend(bindings.resolved.iter().map(|binding| {
        (
            binding.reference.clone(),
            preserved
                .get(&binding.reference.id)
                .unwrap_or(&binding.target)
                .clone(),
        )
    }));
    Ok(())
}
