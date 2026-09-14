#![cfg(feature = "cpp")]

use atlas_engine::{
    CompilerBindingGapReason as Gap, CompilerDeclaration, CompilerDispatch, CompilerLocation,
    CompilerObservation, CompilerObservationKind, ExtractionMode, FileId, GraphBuilder, Language,
    Provenance, ResolutionStrategy, Store, associate_compiler_calls, create_frontend,
    extract_file_with_mode,
};
use std::{collections::BTreeMap, path::Path, sync::Arc};

const SOURCE: &str = "struct Target { int read() { return 1; } int read(int) { return 2; } }; int selected(Target& object) { return object.read(); } int other() { return 0; }";

fn declaration(source: &str, expression: &str, name: &str) -> CompilerDeclaration {
    let start = source.find(expression).unwrap() + expression.find(name).unwrap();
    CompilerDeclaration {
        name: name.into(),
        location: CompilerLocation {
            path: "sample.cpp".into(),
            start_byte: start as u32,
            end_byte: (start + name.len()) as u32,
        },
    }
}

fn case() -> (Arc<Store>, BTreeMap<String, String>, CompilerObservation) {
    let facts = extract_file_with_mode(
        &create_frontend(Language::Cpp).unwrap(),
        FileId::generate("sample.cpp"),
        Path::new("sample.cpp"),
        SOURCE,
        "compiler-input",
        ExtractionMode::Structural,
        &(),
    )
    .unwrap();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.init_schema().unwrap();
    store.insert_file_facts(&facts).unwrap();
    let target = declaration(SOURCE, "read() {", "read");
    let observation = CompilerObservation {
        kind: CompilerObservationKind::CallDeclaration,
        location: declaration(SOURCE, "object.read()", "read").location,
        call_expression: Some(CompilerLocation {
            path: "sample.cpp".into(),
            start_byte: SOURCE.find("object.read()").unwrap() as u32,
            end_byte: (SOURCE.find("object.read()").unwrap() + "object.read()".len()) as u32,
        }),
        declaration: target.clone(),
        definition: Some(target),
        owner: Some(declaration(SOURCE, "selected(Target", "selected")),
        dispatch: CompilerDispatch::Direct,
    };
    (
        store,
        BTreeMap::from([("sample.cpp".into(), "compiler-input".into())]),
        observation,
    )
}

fn virtual_case() -> (Store, BTreeMap<String, String>, CompilerObservation) {
    let source = "struct Base { virtual int run() = 0; virtual int run(int) = 0; }; struct Child : Base { int run() override { return 1; } int run(int) override { return 2; } }; int entry(Base& object) { return object.run(); }";
    let facts = extract_file_with_mode(
        &create_frontend(Language::Cpp).unwrap(),
        FileId::generate("sample.cpp"),
        Path::new("sample.cpp"),
        source,
        "virtual-input",
        ExtractionMode::Structural,
        &(),
    )
    .unwrap();
    let store = Store::open_in_memory().unwrap();
    store.init_schema().unwrap();
    store.insert_file_facts(&facts).unwrap();
    let written = "object.run()";
    let start = source.find(written).unwrap() as u32;
    let observation = CompilerObservation {
        kind: CompilerObservationKind::CallDeclaration,
        location: declaration(source, written, "run").location,
        call_expression: Some(CompilerLocation {
            path: "sample.cpp".into(),
            start_byte: start,
            end_byte: start + written.len() as u32,
        }),
        declaration: declaration(source, "run() = 0", "run"),
        definition: None,
        owner: Some(declaration(source, "entry(Base", "entry")),
        dispatch: CompilerDispatch::Virtual,
    };
    (
        store,
        BTreeMap::from([("sample.cpp".into(), "virtual-input".into())]),
        observation,
    )
}

#[test]
fn virtual_declarations_remain_navigable_without_selecting_an_override() {
    let (store, inputs, observation) = virtual_case();
    let before = store.get_all_call_references().unwrap();
    for owner_available in [true, false] {
        let mut selected = observation.clone();
        if !owner_available {
            selected.owner = None;
        }
        // Even a supplied body must not replace the statically selected
        // declaration or become a runtime call edge for virtual dispatch.
        let body = store
            .get_all_symbols()
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.qualified_name == "Child::run")
            .unwrap();
        selected.definition = Some(CompilerDeclaration {
            name: body.name,
            location: CompilerLocation {
                path: "sample.cpp".into(),
                start_byte: body.name_range.start_byte,
                end_byte: body.name_range.end_byte,
            },
        });
        let report = associate_compiler_calls(&store, &[selected], &inputs, &mut || false).unwrap();
        assert!(report.resolved.is_empty());
        assert_eq!(report.gaps.len(), 1);
        let gap = &report.gaps[0];
        assert_eq!(gap.reason, Gap::DynamicDispatch);
        assert!(gap.reference_id.is_some());
        assert_eq!(
            gap.declaration_location.as_ref(),
            Some(&observation.declaration.location)
        );
        let declaration = store
            .find_symbol_by_id(&gap.declaration_id.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(declaration.qualified_name, "Base::run");
        assert_eq!(
            declaration.name_range.start_byte,
            observation.declaration.location.start_byte
        );
        assert_eq!(declaration.range, declaration.name_range);
        assert_ne!(declaration.id, body.id);
        assert_eq!(before, store.get_all_call_references().unwrap());
        assert!(store.get_all_edges().unwrap().is_empty());
    }
}

#[test]
fn indexed_virtual_declaration_binding_needs_matching_invocation_and_declaration_locations() {
    let (store, inputs, observation) = virtual_case();
    for change in 0..4 {
        let mut selected = observation.clone();
        let mut inputs = inputs.clone();
        match change {
            0 => selected.declaration.location.start_byte += 1,
            1 => selected.declaration.location.path = "absent.cpp".into(),
            2 => selected.call_expression.as_mut().unwrap().end_byte += 1,
            _ => {
                inputs.insert("sample.cpp".into(), "changed".into());
            }
        }
        let report = associate_compiler_calls(&store, &[selected], &inputs, &mut || false).unwrap();
        assert!(report.resolved.is_empty());
        assert!(!report.gaps.is_empty());
        assert!(report.gaps.iter().all(|gap| gap.declaration_id.is_none()));
    }
}

#[test]
fn conflicting_virtual_observations_keep_separate_declaration_clues() {
    let (store, inputs, first) = virtual_case();
    let mut second = first.clone();
    let alternative = store
        .get_all_symbols()
        .unwrap()
        .into_iter()
        .find(|symbol| {
            symbol.qualified_name == "Base::run"
                && symbol.name_range.start_byte != first.declaration.location.start_byte
        })
        .unwrap();
    second.declaration.location.start_byte = alternative.name_range.start_byte;
    second.declaration.location.end_byte = alternative.name_range.end_byte;
    let report =
        associate_compiler_calls(&store, &[first, second], &inputs, &mut || false).unwrap();
    assert!(report.resolved.is_empty());
    assert_eq!(report.gaps.len(), 2);
    assert!(
        report
            .gaps
            .iter()
            .all(|gap| gap.reason == Gap::DynamicDispatch && gap.declaration_id.is_some())
    );
    assert_ne!(report.gaps[0].declaration_id, report.gaps[1].declaration_id);
    assert_ne!(
        report.gaps[0].declaration_location,
        report.gaps[1].declaration_location
    );
    assert!(store.get_all_edges().unwrap().is_empty());
}

#[test]
fn virtual_declaration_source_survives_missing_symbols_and_unassociated_calls() {
    // The declaration is introduced by a C++ macro. Its source spelling is
    // available even when syntax extraction has no symbol at that position.
    let source = "#define MEMBER(name) virtual int name() = 0;\nstruct Base { MEMBER(run) };\nint entry(Base& value) { return value.run(); }\n";
    let facts = extract_file_with_mode(
        &create_frontend(Language::Cpp).unwrap(),
        FileId::generate("sample.cpp"),
        Path::new("sample.cpp"),
        source,
        "macro-input",
        ExtractionMode::Structural,
        &(),
    )
    .unwrap();
    let store = Store::open_in_memory().unwrap();
    store.init_schema().unwrap();
    store.insert_file_facts(&facts).unwrap();
    let location = declaration(source, "MEMBER(run)", "run").location;
    assert!(
        !store
            .get_all_symbols()
            .unwrap()
            .iter()
            .any(|s| s.name_range.start_byte == location.start_byte
                && s.name_range.end_byte == location.end_byte)
    );
    let start = source.find("value.run()").unwrap() as u32;
    let observation = CompilerObservation {
        kind: CompilerObservationKind::CallDeclaration,
        dispatch: CompilerDispatch::Virtual,
        location: declaration(source, "value.run()", "run").location,
        call_expression: Some(CompilerLocation {
            path: "sample.cpp".into(),
            start_byte: start,
            end_byte: start + 11,
        }),
        declaration: CompilerDeclaration {
            name: "run".into(),
            location: location.clone(),
        },
        definition: None,
        owner: Some(declaration(source, "entry(Base", "entry")),
    };
    let inputs = BTreeMap::from([("sample.cpp".into(), "macro-input".into())]);
    for call_available in [true, false] {
        let mut observation = observation.clone();
        if !call_available {
            observation.call_expression = None;
        }
        let report =
            associate_compiler_calls(&store, &[observation], &inputs, &mut || false).unwrap();
        assert!(report.resolved.is_empty());
        assert_eq!(report.gaps.len(), 1);
        let gap = &report.gaps[0];
        assert_eq!(
            gap.reason,
            if call_available {
                Gap::DynamicDispatch
            } else {
                Gap::CallExpressionUnavailable
            }
        );
        assert_eq!(gap.reference_id.is_some(), call_available);
        assert!(gap.declaration_id.is_none());
        assert_eq!(gap.declaration_location.as_ref(), Some(&location));
    }
    for change in 0..4 {
        let mut observation = observation.clone();
        let mut inputs = inputs.clone();
        match change {
            0 => observation.declaration.location.path = "absent.cpp".into(),
            1 => observation.declaration.location.end_byte = location.start_byte,
            2 => {
                inputs.insert("sample.cpp".into(), "different-input".into());
            }
            _ => {
                inputs.clear();
            }
        }
        let report =
            associate_compiler_calls(&store, &[observation], &inputs, &mut || false).unwrap();
        assert!(report.resolved.is_empty());
        assert!(!report.gaps.is_empty());
        assert!(report.gaps.iter().all(|g| g.declaration_location.is_none()));
    }
    assert!(store.get_all_edges().unwrap().is_empty());
}

#[test]
fn direct_calls_reuse_indexed_identity_and_existing_graph_builder() {
    let (store, inputs, observation) = case();
    let before = store.get_all_call_references().unwrap();
    let report = associate_compiler_calls(
        &store,
        &[observation.clone(), observation],
        &inputs,
        &mut || false,
    )
    .unwrap();
    assert!(report.gaps.is_empty(), "{:?}", report.gaps);
    assert_eq!(report.resolved.len(), 1);
    assert_eq!(before, store.get_all_call_references().unwrap());
    assert!(store.get_all_edges().unwrap().is_empty());
    let binding = &report.resolved[0];
    assert_eq!(binding.target.strategy, ResolutionStrategy::Compiler);
    assert_eq!(binding.target.provenance, Provenance::Compiler);
    let target = store
        .find_symbol_by_id(&binding.target.symbol_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        target.name_range.start_byte,
        SOURCE.find("read()").unwrap() as u32
    );
    // Explicit staging consumer: association itself never writes a Ready graph.
    GraphBuilder::new(store.clone())
        .build_all(&[(binding.reference.clone(), binding.target.clone())]);
    let edges = store.get_all_edges().unwrap();
    let calls: Vec<_> = edges
        .iter()
        .filter(|edge| edge.kind.as_str() == "calls")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].ref_id, Some(binding.reference.id));
    assert_eq!(calls[0].target, binding.target.symbol_id);
    assert_eq!(calls[0].provenance, Provenance::Compiler);
    assert_eq!(calls[0].resolved_by, Some(ResolutionStrategy::Compiler));
}

#[test]
fn qualified_callee_spans_match_by_the_complete_invocation() {
    let source = "struct Parent { virtual int read() { return 1; } }; int selected(Parent& object) { return object.Parent::read(); }";
    let written = "object.Parent::read()";
    let facts = extract_file_with_mode(
        &create_frontend(Language::Cpp).unwrap(),
        FileId::generate("sample.cpp"),
        Path::new("sample.cpp"),
        source,
        "compiler-input",
        ExtractionMode::Structural,
        &(),
    )
    .unwrap();
    let store = Store::open_in_memory().unwrap();
    store.init_schema().unwrap();
    store.insert_file_facts(&facts).unwrap();
    let target = declaration(source, "read() {", "read");
    let mut observation = CompilerObservation {
        kind: CompilerObservationKind::CallDeclaration,
        location: declaration(source, written, "read").location,
        call_expression: Some(CompilerLocation {
            path: "sample.cpp".into(),
            start_byte: source.find(written).unwrap() as u32,
            end_byte: (source.find(written).unwrap() + written.len()) as u32,
        }),
        declaration: target.clone(),
        definition: Some(target),
        owner: Some(declaration(source, "selected(Parent", "selected")),
        // Independently checked with libclang: qualification suppresses virtual dispatch.
        dispatch: CompilerDispatch::Direct,
    };
    let inputs = BTreeMap::from([("sample.cpp".into(), "compiler-input".into())]);
    let report =
        associate_compiler_calls(&store, &[observation.clone()], &inputs, &mut || false).unwrap();
    assert_eq!(report.resolved.len(), 1, "{:?}", report.gaps);
    let callsite = store
        .find_callsite_by_reference_id(&report.resolved[0].reference.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        callsite.range.start_byte,
        source.find(written).unwrap() as u32
    );
    observation.call_expression.as_mut().unwrap().end_byte += 1;
    let report =
        associate_compiler_calls(&store, &[observation.clone()], &inputs, &mut || false).unwrap();
    assert!(report.resolved.is_empty());
    assert_eq!(report.gaps[0].reason, Gap::CallsiteNotUnique);
    observation.call_expression = None;
    let report = associate_compiler_calls(&store, &[observation], &inputs, &mut || false).unwrap();
    assert_eq!(report.gaps[0].reason, Gap::CallExpressionUnavailable);
}

#[test]
fn missing_or_changed_inputs_and_inexact_positions_cannot_borrow_symbols() {
    let (store, mut inputs, observation) = case();
    inputs.insert("sample.cpp".into(), "changed".into());
    let report =
        associate_compiler_calls(&store, &[observation.clone()], &inputs, &mut || false).unwrap();
    assert!(report.resolved.is_empty());
    assert_eq!(report.gaps[0].reason, Gap::IndexedInputMismatch);
    inputs.clear();
    let report =
        associate_compiler_calls(&store, &[observation.clone()], &inputs, &mut || false).unwrap();
    assert_eq!(report.gaps[0].reason, Gap::InputIdentityUnavailable);
    inputs.insert("sample.cpp".into(), "compiler-input".into());
    let mut wrong = observation.clone();
    wrong.definition.as_mut().unwrap().location.start_byte += 1;
    let report = associate_compiler_calls(&store, &[wrong], &inputs, &mut || false).unwrap();
    assert!(report.resolved.is_empty());
    assert_eq!(report.gaps[0].reason, Gap::SymbolNotUnique);
    let mut wrong = observation;
    wrong.location.start_byte -= 1;
    let report = associate_compiler_calls(&store, &[wrong], &inputs, &mut || false).unwrap();
    assert_eq!(report.gaps[0].reason, Gap::CallsiteNotUnique);
}

#[test]
fn unknown_dynamic_and_wrong_owners_do_not_become_direct_edges() {
    let (store, inputs, observation) = case();
    let mut variants = Vec::new();
    let mut changed = observation.clone();
    changed.dispatch = CompilerDispatch::Virtual;
    variants.push((changed, Gap::DynamicDispatch));
    let mut changed = observation.clone();
    changed.owner = None;
    variants.push((changed, Gap::OwnerUnavailable));
    let mut changed = observation;
    changed.owner = Some(declaration(SOURCE, "other()", "other"));
    variants.push((changed, Gap::CallerMismatch));
    for (changed, expected) in variants {
        let report = associate_compiler_calls(&store, &[changed], &inputs, &mut || false).unwrap();
        assert!(report.resolved.is_empty());
        assert_eq!(report.gaps[0].reason, expected);
    }
    assert!(store.get_all_edges().unwrap().is_empty());
}

#[test]
fn selected_declarations_survive_missing_definitions_without_guessing_a_body() {
    let source = "int external(int); int external(double); int selected(int value) { return external(value); }";
    let facts = extract_file_with_mode(
        &create_frontend(Language::Cpp).unwrap(),
        FileId::generate("sample.cpp"),
        Path::new("sample.cpp"),
        source,
        "declaration-input",
        ExtractionMode::Structural,
        &(),
    )
    .unwrap();
    let store = Store::open_in_memory().unwrap();
    store.init_schema().unwrap();
    store.insert_file_facts(&facts).unwrap();
    let observation = CompilerObservation {
        kind: CompilerObservationKind::CallDeclaration,
        location: declaration(source, "external(value)", "external").location,
        call_expression: Some(CompilerLocation {
            path: "sample.cpp".into(),
            start_byte: source.find("external(value)").unwrap() as u32,
            end_byte: (source.find("external(value)").unwrap() + "external(value)".len()) as u32,
        }),
        declaration: declaration(source, "external(int)", "external"),
        definition: None,
        owner: Some(declaration(source, "selected(int", "selected")),
        dispatch: CompilerDispatch::Direct,
    };
    let inputs = BTreeMap::from([("sample.cpp".into(), "declaration-input".into())]);
    let report =
        associate_compiler_calls(&store, &[observation.clone()], &inputs, &mut || false).unwrap();
    assert_eq!(report.resolved.len(), 1, "{:?}", report.gaps);
    let binding = &report.resolved[0];
    let target = store
        .find_symbol_by_id(&binding.target.symbol_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        target.name_range.start_byte,
        source.find("external(int)").unwrap() as u32
    );
    assert_eq!(report.gaps.len(), 1);
    assert_eq!(report.gaps[0].reason, Gap::DefinitionUnavailable);
    assert_eq!(report.gaps[0].reference_id, Some(binding.reference.id));
    assert!(
        store
            .find_callsites_by_file(&target.file_id)
            .unwrap()
            .iter()
            .all(|call| call.caller != target.id)
    );
    assert!(store.get_all_edges().unwrap().is_empty());

    // An explicitly supplied invalid definition is not silently replaced by
    // the declaration; an unindexed declaration cannot become a guessed edge.
    for bad_definition in [false, true] {
        let mut invalid = observation.clone();
        if bad_definition {
            let mut definition = invalid.declaration.clone();
            definition.location.start_byte += 1;
            invalid.definition = Some(definition);
        } else {
            invalid.declaration.location.start_byte += 1;
        }
        let report = associate_compiler_calls(&store, &[invalid], &inputs, &mut || false).unwrap();
        assert!(report.resolved.is_empty());
        assert_eq!(report.gaps[0].reason, Gap::SymbolNotUnique);
    }
}

#[test]
fn conflicting_or_incomplete_observations_at_one_call_keep_ambiguity() {
    let (store, inputs, observation) = case();
    let mut other = observation.clone();
    other.definition = Some(declaration(SOURCE, "read(int)", "read"));
    for observations in [
        [observation.clone(), other.clone()],
        [other, observation.clone()],
    ] {
        let report =
            associate_compiler_calls(&store, &observations, &inputs, &mut || false).unwrap();
        assert!(report.resolved.is_empty());
        assert_eq!(report.gaps[0].reason, Gap::ConflictingTargets);
    }
    let mut incomplete = observation.clone();
    incomplete.dispatch = CompilerDispatch::Virtual;
    let report =
        associate_compiler_calls(&store, &[observation, incomplete], &inputs, &mut || false)
            .unwrap();
    assert!(report.resolved.is_empty());
    assert_eq!(report.gaps[0].reason, Gap::DynamicDispatch);
}

#[test]
fn field_observations_and_cancelled_batches_do_not_mutate_calls() {
    let (store, inputs, mut observation) = case();
    let before = store.get_all_call_references().unwrap();
    observation.kind = CompilerObservationKind::FieldReference;
    observation.dispatch = CompilerDispatch::NotACall;
    let report =
        associate_compiler_calls(&store, &[observation.clone()], &inputs, &mut || false).unwrap();
    assert!(report.resolved.is_empty());
    assert!(associate_compiler_calls(&store, &[observation], &inputs, &mut || true).is_err());
    assert_eq!(before, store.get_all_call_references().unwrap());
}
