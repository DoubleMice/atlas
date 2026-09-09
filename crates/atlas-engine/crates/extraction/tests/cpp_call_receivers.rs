#![cfg(feature = "cpp")]

use std::path::Path;

use extraction::{ExtractionMode, create_frontend, extract_file_with_mode};
use types::{FileId, ReferenceKind, SymbolKind};

#[test]
fn cpp_data_fields_and_pointer_returning_methods_remain_distinct() {
    let source = "class Owner { int count_; Device* device_; Device& ref_; Box<Device> wrapped_; int (*callback_)(int); Device* get(); void run(); };";
    let facts = extract_file_with_mode(
        &create_frontend(types::Language::Cpp).unwrap(),
        FileId::generate("fields.cpp"),
        Path::new("fields.cpp"),
        source,
        "fixture",
        ExtractionMode::Structural,
        &(),
    )
    .unwrap();
    for (name, expected) in [
        ("count_", SymbolKind::Field),
        ("device_", SymbolKind::Field),
        ("ref_", SymbolKind::Field),
        ("wrapped_", SymbolKind::Field),
        ("callback_", SymbolKind::Field),
        ("get", SymbolKind::Method),
        ("run", SymbolKind::Method),
    ] {
        let found: Vec<_> = facts.symbols.iter().filter(|s| s.name == name).collect();
        assert_eq!(found.len(), 1, "{name}: {found:#?}");
        assert_eq!(found[0].kind, expected, "{name}");
        assert_eq!(found[0].qualified_name, format!("Owner::{name}"));
        assert_eq!(
            &source[found[0].name_range.start_byte as usize..found[0].name_range.end_byte as usize],
            name
        );
    }
}

#[test]
fn cpp_calls_preserve_receivers_arguments_and_nested_calls() {
    let source = r#"
void run() {
    peer->run(1, nested(2));
    object.run(/* comment */ 3);
    this->run();
    ns::Utility::run(4);
    (factory())->run(5);
}
template<class... Args> void forward(Args... args) { target(args...); }
"#;
    let facts = extract_file_with_mode(
        &create_frontend(types::Language::Cpp).unwrap(),
        FileId::generate("calls.cpp"),
        Path::new("calls.cpp"),
        source,
        "fixture",
        ExtractionMode::Structural,
        &(),
    )
    .unwrap();
    for (text, receiver, arity) in [
        ("peer->run", Some("peer"), Some(2)),
        ("object.run", Some("object"), Some(1)),
        ("this->run", Some("this"), Some(0)),
        ("ns::Utility::run", Some("ns::Utility"), Some(1)),
        ("(factory())->run", Some("(factory())"), Some(1)),
        ("nested", None, Some(1)),
        ("factory", None, Some(0)),
        ("target", None, None),
    ] {
        let reference = facts
            .references
            .iter()
            .find(|r| r.kind == ReferenceKind::Call && r.text == text)
            .unwrap_or_else(|| panic!("missing call {text}: {:?}", facts.references));
        assert_eq!(reference.receiver.as_deref(), receiver, "{text}");
        assert_eq!(reference.arity, arity, "{text}");
        let callsite = facts
            .callsites
            .iter()
            .find(|c| c.reference_id == Some(reference.id))
            .unwrap();
        assert_eq!(callsite.receiver.as_deref(), receiver, "{text}");
        if let Some(arity) = arity {
            assert_eq!(callsite.args.len(), arity as usize, "{text}");
        }
        if text == "peer->run" {
            assert_eq!(callsite.args[1].value, "nested(2)");
        }
    }
}
