use std::{collections::BTreeMap, path::Path, sync::Arc};

use db::Store;
use extraction::{ExtractionMode, create_frontend, extract_file_with_mode};
use resolution::ReferenceResolver;
use types::{FileId, Language, ReferenceKind};

fn analyze(files: &[(&str, &str)], scoped: bool) -> BTreeMap<String, Option<String>> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.init_schema().unwrap();
    let mut file_ids = Vec::new();
    for (path, source) in files {
        let id = FileId::generate(path);
        file_ids.push(id);
        let facts = extract_file_with_mode(
            &create_frontend(Language::Cpp).unwrap(),
            id,
            Path::new(path),
            source,
            "fixture",
            ExtractionMode::Structural,
            &(),
        )
        .unwrap();
        store.insert_file_facts(&facts).unwrap();
    }
    let mut resolver = ReferenceResolver::new(store.clone());
    let (resolved, _) = if scoped {
        resolver
            .resolve_for_closure("cpp-test", 0, &file_ids, None)
            .unwrap()
    } else {
        resolver
            .resolve_all_parallel(store.clone(), None, None)
            .unwrap()
    };
    let symbols: BTreeMap<_, _> = store
        .get_all_symbols()
        .unwrap()
        .into_iter()
        .map(|s| (s.id, s.qualified_name))
        .collect();
    let targets: BTreeMap<_, _> = resolved
        .into_iter()
        .map(|(r, t)| (r.id, symbols[&t.symbol_id].clone()))
        .collect();
    let mut calls = BTreeMap::new();
    for file in file_ids {
        for reference in store.find_references_by_file(&file).unwrap() {
            if reference.kind == ReferenceKind::Call {
                let caller = reference
                    .source_symbol
                    .map(|id| symbols[&id].as_str())
                    .unwrap_or("?");
                let key = format!("{caller}:{}", reference.text);
                calls.insert(key, targets.get(&reference.id).cloned());
            }
        }
    }
    calls
}

#[test]
fn cpp_targets_respect_receivers_qualified_scopes_arity_and_ambiguity() {
    let source = r#"
namespace left { class Tool { public: static int run(int value) { return value; } }; }
namespace right { class Tool { public: static int run(int value) { return -value; } }; }
namespace demo {
class Driver { public: Driver* other; int step(int value); };
int Driver::step(int value) { other->step(value); return step(value); }
int recurse(int value) { return recurse(value - 1); }
int zero() { return 0; }
int overloaded(int value) { return value; }
int overloaded(double value) { return 0; }
int callback(int value) { return value; }
int shadow(int callback) { return callback(1); }
int shadow_pointer(int (*callback)(int)) { return callback(1); }
int inherited(int value) { return value; }
class Base { public: int inherited(int value) { return value; } };
class Child : public Base { public: int call(int value) { return inherited(value); } };
template<class T> int templated(T value) { return value; }
int caller() {
    left::Tool::run(1);
    right::Tool::run(1);
    Ghost::run(1);
    zero(1);
    overloaded(1);
    templated(1);
    return 0;
}
}
class Unrelated { public: int step(int value) { return value; } };
"#;
    let global = analyze(&[("main.cpp", source)], false);
    assert_eq!(global, analyze(&[("main.cpp", source)], true));
    for (call, target) in [
        ("demo::Driver::step:other->step", None),
        ("demo::Driver::step:step", Some("demo::Driver::step")),
        ("demo::recurse:recurse", Some("demo::recurse")),
        ("demo::caller:left::Tool::run", Some("left::Tool::run")),
        ("demo::caller:right::Tool::run", Some("right::Tool::run")),
        ("demo::caller:Ghost::run", None),
        ("demo::caller:zero", None),
        ("demo::caller:overloaded", None),
        ("demo::shadow:callback", None),
        ("demo::shadow_pointer:callback", None),
        ("demo::Child::call:inherited", None),
        ("demo::caller:templated", None),
    ] {
        assert_eq!(
            global.get(call).map(|target| target.as_deref()),
            Some(target),
            "{call}: {global:#?}"
        );
    }
}

#[test]
fn cpp_visible_declarations_link_to_unique_bodies_without_directory_tie_breaking() {
    let header = "namespace api { int process(int value); class Gate { public: static int check(int mode = 0); }; }";
    let implementation = "namespace api { int process(int value) { return value; } int Gate::check(int mode) { return mode; } }";
    let caller =
        "#include \"api.hpp\"\nint call() { return api::process(1) + api::Gate::check(); }";
    let unrelated = "namespace other { int process(int value) { return -value; } int Gate::check(int mode) { return -mode; } }";
    let mut baseline = None;
    for unrelated_path in ["near.cpp", "distant/other.cpp"] {
        let files = [
            ("api.hpp", header),
            ("impl/source.cpp", implementation),
            ("main.cpp", caller),
            (unrelated_path, unrelated),
        ];
        let result = analyze(&files, false);
        assert_eq!(result, analyze(&files, true));
        assert_eq!(
            result.get("call:api::process").unwrap().as_deref(),
            Some("api::process")
        );
        assert_eq!(
            result.get("call:api::Gate::check").unwrap().as_deref(),
            Some("api::Gate::check")
        );
        if let Some(baseline) = &baseline {
            assert_eq!(&result, baseline);
        }
        baseline = Some(result);
    }
    let missing = analyze(
        &[("main.cpp", caller), ("impl/source.cpp", implementation)],
        false,
    );
    assert_eq!(missing.get("call:api::process"), Some(&None));
    assert_eq!(missing.get("call:api::Gate::check"), Some(&None));
}

#[test]
fn qualified_ordinary_functions_accept_nested_template_parameter_types_without_guessing_overloads()
{
    let header = r#"
template<class T> struct Box {};
template<class A, class B> struct Pair {};
namespace api {
void accept(const Box<Pair<int, Box<double>>>& value, int mode = 0);
void overloaded(Box<int> value);
void overloaded(Box<double> value);
void callback(void (*fn)(int));
}
"#;
    let body =
        "namespace api { void accept(const Box<Pair<int, Box<double>>>& value, int mode) {} }";
    let source = r#"#include "api.hpp"
void entry(Box<Pair<int, Box<double>>> value, Box<int> other) {
    api::accept(value);
    api::overloaded(other);
    api::callback(nullptr);
}
namespace api { void unqualified(Box<Pair<int, Box<double>>> value) { accept(value); } }
"#;
    let files = [
        ("api.hpp", header),
        ("body.cpp", body),
        ("main.cpp", source),
        (
            "noise.cpp",
            "namespace other { void accept(Box<int> value) {} }",
        ),
    ];
    let result = analyze(&files, false);
    assert_eq!(result, analyze(&files, true));
    assert_eq!(result["entry:api::accept"].as_deref(), Some("api::accept"));
    assert_eq!(result["entry:api::overloaded"], None);
    assert_eq!(result["entry:api::callback"], None);
    assert_eq!(result["api::unqualified:accept"], None);
    let missing_header = analyze(&[("main.cpp", source), ("body.cpp", body)], false);
    assert_eq!(missing_header["entry:api::accept"], None);
}
