#![cfg(feature = "cpp")]

use extraction::{ExtractionMode, create_frontend, extract_file_with_mode};
use std::path::Path;
use types::{FileFacts, FileId, Language, ReferenceKind, SymbolKind};

fn extract(source: &str, mode: ExtractionMode) -> FileFacts {
    extract_file_with_mode(
        &create_frontend(Language::Cpp).unwrap(),
        FileId::generate("table.cpp"),
        Path::new("table.cpp"),
        source,
        "fixed",
        mode,
        &(),
    )
    .unwrap()
}

#[test]
fn macro_table_declarations_keep_their_written_namespace_and_primary_directives() {
    let enumeration = "enum Id {\n#define ITEM(name, id) k##name = id,\n#include \"items.inl\"\n#undef ITEM\nCount\n};\n";
    let names = "const char* names[] = {\n#define ITEM(name, id) #name,\n#include \"items.inl\"\n#undef ITEM\n};\n";
    let values =
        "int values[] = {\n#define ITEM(name, id) id,\n#include \"items.inl\"\n#undef ITEM\n};\n";
    for parts in [
        vec![enumeration],
        vec![names],
        vec![values],
        vec![enumeration, names],
        vec![names, values],
        vec![enumeration, names, values],
    ] {
        for ending in ["\n", "\r\n"] {
            let source = format!(
                "// 非 ASCII 注释\nnamespace product {{ namespace render {{ namespace detail {{\n{}\nclass Worker {{ public: void run(); }};\nvoid Worker::run() {{}}\n}}}}}}\nnamespace unrelated {{ class Worker; }}\n",
                parts.join("\n")
            )
            .replace('\n', ending);
            for mode in [ExtractionMode::Structural, ExtractionMode::Full] {
                let facts = extract(&source, mode);
                let class = facts
                    .symbols
                    .iter()
                    .find(|s| {
                        s.kind == SymbolKind::Class
                            && s.name_range.start_byte
                                < source.find("namespace unrelated").unwrap() as u32
                    })
                    .unwrap();
                assert_eq!(
                    class.qualified_name, "product::render::detail::Worker",
                    "{source}"
                );
                let methods: Vec<_> = facts.symbols.iter().filter(|s| s.name == "run").collect();
                assert_eq!(methods.len(), 2);
                assert!(
                    methods
                        .iter()
                        .all(|s| s.qualified_name == "product::render::detail::Worker::run")
                );
                assert!(
                    facts
                        .symbols
                        .iter()
                        .any(|s| s.qualified_name == "unrelated::Worker")
                );
                assert_eq!(facts.imports.len(), parts.len());
                let macros = &facts.cpp_types.as_ref().unwrap().macros;
                assert_eq!(macros.len(), parts.len() * 2);
                for definition in macros {
                    let text = &source
                        [definition.range.start_byte as usize..definition.range.end_byte as usize];
                    assert!(text.starts_with("#define ITEM") || text.starts_with("#undef ITEM"));
                    assert_eq!(definition.name, "ITEM");
                    assert_eq!(
                        definition.replacement.is_some(),
                        text.starts_with("#define")
                    );
                }
                assert_eq!(
                    facts
                        .symbols
                        .iter()
                        .filter(|s| s.kind == SymbolKind::Macro)
                        .count(),
                    parts.len()
                );
                for symbol in &facts.symbols {
                    assert_eq!(
                        &source[symbol.name_range.start_byte as usize
                            ..symbol.name_range.end_byte as usize],
                        symbol.name
                    );
                }
            }
        }
    }
}

#[test]
fn recovery_preserves_literal_branch_definitions_and_ignores_directive_text() {
    let source = r###"
namespace sample {
enum Kind {
#if 0
#define CHOICE 10
    A,
#else
#define CHOICE 20
    B,
#endif
    C
};
const char* text = R"tag(
#define FAKE 1
} namespace false_scope {
)tag";
/*
#define ALSO_FAKE 1
*/
class Worker {};
}
"###;
    for mode in [ExtractionMode::Structural, ExtractionMode::Full] {
        let facts = extract(source, mode.clone());
        assert!(
            facts
                .symbols
                .iter()
                .any(|s| s.qualified_name == "sample::Worker")
        );
        let macros = &facts.cpp_types.as_ref().unwrap().macros;
        assert_eq!(macros.len(), 1, "{macros:?}");
        assert_eq!(macros[0].name, "CHOICE");
        assert_eq!(macros[0].replacement.as_deref(), Some("20"));
        assert!(
            !extract("namespace broken { class Worker {};", mode)
                .diagnostics
                .is_empty()
        );
    }
}

#[test]
fn directives_between_initializer_values_preserve_actual_calls() {
    let source = r#"
namespace pipeline {
int read();
int values[] = {
#define FIRST 1
    read()
#define NEXT(x) not_a_call(x)
    ,
#undef FIRST
    read(),
#undef NEXT
};
class After {};
}
"#;
    for mode in [ExtractionMode::Structural, ExtractionMode::Full] {
        let facts = extract(source, mode);
        assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
        assert!(
            facts
                .symbols
                .iter()
                .any(|s| s.qualified_name == "pipeline::After")
        );
        let calls: Vec<_> = facts
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|r| r.name == "read"));
        for call in calls {
            assert_eq!(
                &source[call.range.start_byte as usize..call.range.end_byte as usize],
                "read"
            );
        }
        assert_eq!(facts.cpp_types.as_ref().unwrap().macros.len(), 4);
    }
}

#[test]
fn directives_do_not_supply_missing_list_separators_or_closing_braces() {
    for source in [
        "enum E { A\n#define GAP 1\nB };",
        "int values[] = { 1\n#undef GAP\n2 };",
        "namespace broken { enum E {\n#define X 1\nA, }; class After {};",
    ] {
        let facts = extract(source, ExtractionMode::Structural);
        assert!(
            !facts.diagnostics.is_empty(),
            "invalid source parsed without a gap: {source}"
        );
    }
}
