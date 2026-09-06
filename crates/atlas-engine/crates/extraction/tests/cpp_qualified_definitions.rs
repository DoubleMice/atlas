#![cfg(feature = "cpp")]

use std::path::Path;

use extraction::{ExtractionMode, create_frontend, extract_file_with_mode};
use types::{FileFacts, FileId, Language, SymbolKind};

fn extract(source: &str, mode: ExtractionMode) -> FileFacts {
    extract_file_with_mode(
        &create_frontend(Language::Cpp).unwrap(),
        FileId::generate("qualified.cpp"),
        Path::new("qualified.cpp"),
        source,
        "fixture",
        mode,
        &(),
    )
    .unwrap()
}

#[test]
fn qualified_definitions_keep_owner_name_signature_and_body_range() {
    let source = r#"
namespace OHOS {
namespace Notification {
int NotificationStub::OnRemoteRequest(int code) {
    return HandlePublish(code);
}
int NotificationStub::HandlePublish(int code) { return code; }
int OtherStub::HandlePublish(int code) { return -code; }
const Value* NotificationStub::pointer() { return nullptr; }
const Value& NotificationStub::reference() { return value; }
int Nested::Stub::run() { return 1; }
int Spaced /* owner */ :: run() { return 2; }
}
}
int ::Global::run() { return 3; }
"#;
    let facts = extract(source, ExtractionMode::Structural);
    for (qualified, name, signature) in [
        (
            "OHOS::Notification::NotificationStub::OnRemoteRequest",
            "OnRemoteRequest",
            "(int code): int",
        ),
        (
            "OHOS::Notification::NotificationStub::HandlePublish",
            "HandlePublish",
            "(int code): int",
        ),
        (
            "OHOS::Notification::OtherStub::HandlePublish",
            "HandlePublish",
            "(int code): int",
        ),
        (
            "OHOS::Notification::NotificationStub::pointer",
            "pointer",
            "(): const Value",
        ),
        (
            "OHOS::Notification::NotificationStub::reference",
            "reference",
            "(): const Value",
        ),
        ("OHOS::Notification::Nested::Stub::run", "run", "(): int"),
        ("OHOS::Notification::Spaced::run", "run", "(): int"),
        ("Global::run", "run", "(): int"),
    ] {
        let symbols: Vec<_> = facts
            .symbols
            .iter()
            .filter(|s| s.qualified_name == qualified)
            .collect();
        assert_eq!(symbols.len(), 1, "missing or duplicate {qualified}");
        let symbol = symbols[0];
        assert_eq!(symbol.kind, SymbolKind::Function);
        assert_eq!(symbol.name, name);
        assert_eq!(symbol.signature.as_deref(), Some(signature));
        assert_eq!(
            &source[symbol.name_range.start_byte as usize..symbol.name_range.end_byte as usize],
            name
        );
        let body = &source[symbol.range.start_byte as usize..symbol.range.end_byte as usize];
        assert!(body.contains("return "), "{qualified}: {body}");
        assert!(body.ends_with('}'), "{qualified}: {body}");
    }
    let handlers: Vec<_> = facts
        .symbols
        .iter()
        .filter(|s| s.name == "HandlePublish")
        .collect();
    assert_eq!(handlers.len(), 2);
    assert_ne!(
        handlers[0].id, handlers[1].id,
        "different owners must not fold together"
    );
}

#[test]
fn qualified_returns_parameters_and_declarations_are_not_definitions() {
    let source = r#"
Result::Value plain(Arg::Value value) { return value; }
int Stub::declaration(int code);
int Stub::deleted() = delete;
namespace Hidden { int Stub::nested() { return 1; } }
int Outer::Inner::run() { return 2; }
const Value* Stub::pointer() { return nullptr; }
const Value& Stub::reference() { return value; }
"#;
    let structural = extract(source, ExtractionMode::Structural);
    for unexpected in ["Value", "declaration", "deleted"] {
        assert!(
            structural.symbols.iter().all(|s| s.name != unexpected),
            "spurious {unexpected}"
        );
    }
    let manifest = extract(source, ExtractionMode::Manifest);
    let names: Vec<_> = manifest
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect();
    for expected in [
        "plain",
        "Outer::Inner::run",
        "Stub::pointer",
        "Stub::reference",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    assert!(
        !names.contains(&"Hidden::Stub::nested"),
        "manifest must stay at file scope"
    );
}
