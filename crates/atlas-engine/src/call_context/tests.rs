use super::*;
use crate::{ExtractionMode, IndexPipeline, IndexPipelineOptions, NoopSink};

fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Store>) {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.init_schema().unwrap();
    IndexPipeline::new(
        store.clone(),
        root.path().to_path_buf(),
        IndexPipelineOptions::new(ExtractionMode::Structural),
    )
    .run(&NoopSink, &mut || false)
    .unwrap();
    (root, store)
}

fn inspect(root: &Path, store: &Store, source: &str, expression: &str) -> CallContextResult {
    let start = source.find(expression).unwrap() as u32;
    inspect_call_context(
        store,
        root,
        "main.cpp",
        start,
        start + expression.len() as u32,
        &|| false,
    )
    .unwrap()
}

fn item_text(root: &Path, store: &Store, item: &CallContextItem) -> String {
    let file = store.get_file(&item.location.file_id).unwrap().unwrap();
    let source = std::fs::read_to_string(root.join(file.path)).unwrap();
    source[item.location.range.start_byte as usize..item.location.range.end_byte as usize]
        .to_owned()
}

#[test]
fn initializer_return_and_type_context_work_without_wrapper_dependencies_or_graph_writes() {
    let header = "namespace demo { class Device { public: void run(); }; class Root { public: Missing<Device> Make(); void entry(); }; }";
    let source = "#include \"api.hpp\"\nnamespace demo { Missing<Device> Root::Make() { return {}; } void Root::entry() { auto receiver = Make(); receiver->run(); } void Device::run() {} }";
    let unrelated =
        "namespace other { class Device { public: void run(); }; void Device::run() {} }";
    let (root, store) = fixture(&[
        ("main.cpp", source),
        ("api.hpp", header),
        ("other.cpp", unrelated),
    ]);
    let before = serde_json::to_value(store.get_all_call_references().unwrap()).unwrap();
    let edges_before = serde_json::to_value(store.get_all_edges().unwrap()).unwrap();
    let result = inspect(root.path(), &store, source, "receiver->run()");
    let roles: BTreeSet<_> = result.items.iter().map(|item| item.role).collect();
    for role in [
        "receiver_binding",
        "initializer",
        "callable_declaration",
        "type_reference",
        "type_declaration",
        "type_member",
    ] {
        assert!(roles.contains(role), "{role}: {result:#?}");
    }
    assert!(result.items.iter().any(|i| i.role == "receiver_binding"
        && item_text(root.path(), &store, i) == "auto receiver = Make();"));
    assert!(result.items.iter().any(|i| i.role == "callable_declaration"
        && item_text(root.path(), &store, i).contains("Missing<Device> Make()")));
    assert!(
        result
            .gaps
            .iter()
            .any(|g| g.code == "type_declaration_unavailable" && g.message.contains("Missing")),
        "{result:#?}"
    );
    assert!(result.items.iter().all(|i| !i.related_locations.is_empty()));
    // Same-name types in another namespace are still hypotheses, never exclusions.
    let member_names: BTreeSet<_> = result
        .items
        .iter()
        .filter(|i| i.role == "type_member")
        .map(|i| {
            store
                .find_symbol_by_id(&i.symbol_id.unwrap())
                .unwrap()
                .unwrap()
                .qualified_name
        })
        .collect();
    assert!(member_names.contains("demo::Device::run"));
    assert!(member_names.contains("other::Device::run"));
    assert!(
        result.files_read < 3,
        "Only the call/factory source files need parsing: {result:#?}"
    );
    assert_eq!(
        before,
        serde_json::to_value(store.get_all_call_references().unwrap()).unwrap()
    );
    assert_eq!(
        edges_before,
        serde_json::to_value(store.get_all_edges().unwrap()).unwrap()
    );
}

#[test]
fn nearest_scope_and_ambiguous_bindings_retain_distinct_origins() {
    let source = r#"namespace demo {
class A { public: void run(); }; class B { public: void run(); };
A* MakeA(); B* MakeB();
void outer() {
    auto receiver = MakeA();
    { auto receiver = MakeB(); receiver->run(); }
    receiver->run();
}
void ambiguous() { auto value = MakeA(); auto value = MakeB(); value->run(); }
void alias() { auto receiver = another; receiver->run(); }
}"#;
    let (root, store) = fixture(&[("main.cpp", source)]);
    let first = source.find("receiver->run()").unwrap();
    let second = source[first + 1..].find("receiver->run()").unwrap() + first + 1;
    for (offset, wanted, excluded) in [
        (first, "MakeB()", "MakeA()"),
        (second, "MakeA()", "MakeB()"),
    ] {
        let result = inspect_call_context(
            &store,
            root.path(),
            "main.cpp",
            offset as u32,
            (offset + 15) as u32,
            &|| false,
        )
        .unwrap();
        let initializers: Vec<_> = result
            .items
            .iter()
            .filter(|i| i.role == "initializer")
            .map(|i| item_text(root.path(), &store, i))
            .collect();
        assert!(initializers.iter().any(|s| s == wanted), "{result:#?}");
        assert!(!initializers.iter().any(|s| s == excluded), "{result:#?}");
    }
    let ambiguous = inspect(root.path(), &store, source, "value->run()");
    assert!(
        ambiguous
            .gaps
            .iter()
            .any(|g| g.code == "receiver_binding_ambiguous"),
        "{ambiguous:#?}"
    );
    assert_eq!(
        ambiguous
            .items
            .iter()
            .filter(|i| i.role == "receiver_binding")
            .count(),
        2
    );
    let alias_start = source.rfind("receiver->run()").unwrap() as u32;
    let alias = inspect_call_context(
        &store,
        root.path(),
        "main.cpp",
        alias_start,
        alias_start + 15,
        &|| false,
    )
    .unwrap();
    assert!(
        alias
            .gaps
            .iter()
            .any(|g| g.code == "initializer_value_unestablished"),
        "{alias:#?}"
    );
    assert!(alias.items.iter().all(|i| i.role != "callable_declaration"));
}

#[test]
fn explicit_object_pointer_and_reference_declarations_are_available() {
    let source = "class Device { public: void run(); }; void example(Device& input) { Device local; local.run(); Device* pointer = &input; pointer->run(); Device& reference = input; reference.run(); }";
    let (root, store) = fixture(&[("main.cpp", source)]);
    for expression in ["local.run()", "pointer->run()", "reference.run()"] {
        let result = inspect(root.path(), &store, source, expression);
        assert!(
            result.items.iter().any(|i| i.role == "receiver_binding"),
            "{expression}: {result:#?}"
        );
        assert!(
            result.items.iter().any(|i| i.role == "type_member"),
            "{expression}: {result:#?}"
        );
        assert!(
            result
                .gaps
                .iter()
                .any(|g| g.code == "receiver_semantics_unverified")
        );
    }
}

#[test]
fn class_fields_supply_declaration_context_across_files_without_smart_pointer_inference() {
    let header = "namespace demo { class Device { public: void run(); }; class Owner { Missing<Device> device_; public: void entry(); }; }";
    let source = "#include \"api.hpp\"\nnamespace demo { void Owner::entry() { device_->run(); this->device_->run(); } }";
    let noise = "namespace other { class Device { public: void run(); }; class Owner { Missing<Device> device_; }; }";
    let (root, store) = fixture(&[
        ("main.cpp", source),
        ("api.hpp", header),
        ("noise.cpp", noise),
    ]);
    let before = serde_json::to_value(store.get_all_edges().unwrap()).unwrap();
    for expression in ["device_->run()", "this->device_->run()"] {
        let result = inspect(root.path(), &store, source, expression);
        let fields: Vec<_> = result
            .items
            .iter()
            .filter(|i| i.role == "receiver_binding")
            .collect();
        assert_eq!(fields.len(), 1, "{result:#?}");
        assert_eq!(
            item_text(root.path(), &store, fields[0]),
            "Missing<Device> device_;"
        );
        assert_eq!(
            store
                .find_symbol_by_id(&fields[0].symbol_id.unwrap())
                .unwrap()
                .unwrap()
                .qualified_name,
            "demo::Owner::device_"
        );
        assert!(
            result.items.iter().any(|i| i.role == "type_member"),
            "{result:#?}"
        );
        assert!(
            result
                .gaps
                .iter()
                .any(|g| g.code == "type_declaration_unavailable" && g.message.contains("Missing"))
        );
        assert!(
            result
                .gaps
                .iter()
                .any(|g| g.code == "receiver_semantics_unverified")
        );
        assert!(
            !result
                .gaps
                .iter()
                .any(|g| g.code == "receiver_binding_unavailable")
        );
    }
    assert_eq!(
        before,
        serde_json::to_value(store.get_all_edges().unwrap()).unwrap()
    );
}

#[test]
fn lexical_shadowing_and_lambda_captures_do_not_become_class_fields() {
    let source = r#"
class A { public: void run(); }; class B { public: void run(); };
class Owner { A* device_; public: void entry(B* device_) {
    device_->run(); this->device_->run();
} void capture(B* other) { auto work = [device_ = other]() { device_->run(); }; }
};
"#;
    let (root, store) = fixture(&[("main.cpp", source)]);
    let local = inspect(root.path(), &store, source, "device_->run()");
    assert!(
        local
            .items
            .iter()
            .any(|i| i.role == "receiver_binding"
                && item_text(root.path(), &store, i) == "B* device_"),
        "{local:#?}"
    );
    let explicit = inspect(root.path(), &store, source, "this->device_->run()");
    assert!(
        explicit
            .items
            .iter()
            .any(|i| i.role == "receiver_binding"
                && item_text(root.path(), &store, i) == "A* device_;"),
        "{explicit:#?}"
    );
    let offset = source.rfind("device_->run()").unwrap() as u32;
    let captured = inspect_call_context(
        &store,
        root.path(),
        "main.cpp",
        offset,
        offset + 14,
        &|| false,
    )
    .unwrap();
    assert!(
        captured.items.iter().all(|i| i.role != "receiver_binding"),
        "{captured:#?}"
    );
    assert!(
        captured
            .gaps
            .iter()
            .any(|g| g.code == "receiver_capture_unverified")
    );
}

#[test]
fn repeated_class_definitions_remain_ambiguous_and_inherited_fields_are_not_guessed() {
    let source = "class Owner { public: void entry(); }; void Owner::entry() { device_->run(); } class Base { Device* inherited_; }; class Child : public Base { void entry() { inherited_->run(); } };";
    let (root, store) = fixture(&[
        ("main.cpp", source),
        ("variant_a.hpp", "class Owner { DeviceA* device_; };"),
        ("variant_b.hpp", "class Owner { DeviceB* device_; };"),
    ]);
    let result = inspect(root.path(), &store, source, "device_->run()");
    assert_eq!(
        result
            .items
            .iter()
            .filter(|i| i.role == "receiver_binding")
            .count(),
        2,
        "{result:#?}"
    );
    assert!(
        result
            .gaps
            .iter()
            .any(|g| g.code == "receiver_binding_ambiguous")
    );
    let inherited = inspect(root.path(), &store, source, "inherited_->run()");
    assert!(inherited.items.iter().all(|i| i.role != "receiver_binding"));
    assert!(
        inherited
            .gaps
            .iter()
            .any(|g| g.code == "receiver_binding_unavailable")
    );
}

#[test]
fn source_failure_and_cancellation_do_not_report_context_completion() {
    let source = "void example() { auto value = Make(); value->run(); }";
    let (root, store) = fixture(&[("main.cpp", source)]);
    assert!(
        inspect_call_context(
            &store,
            root.path(),
            "main.cpp",
            0,
            source.len() as u32,
            &|| true
        )
        .is_err()
    );
    std::fs::remove_file(root.path().join("main.cpp")).unwrap();
    let result = inspect(root.path(), &store, source, "value->run()");
    assert!(result.items.is_empty());
    assert!(
        result
            .gaps
            .iter()
            .any(|g| g.code == "source_context_unavailable")
    );
}
