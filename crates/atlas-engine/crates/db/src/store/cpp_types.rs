use std::collections::{HashMap, HashSet};

use rusqlite::OptionalExtension;
use types::{
    FileId,
    cpp::{CppFileTypes, CppLookupLimit},
};

use super::Store;

impl Store {
    pub fn cpp_allocation_site_count(&self) -> anyhow::Result<usize> {
        let count: i64 = self.lock_read().query_row(
            "SELECT coalesce(sum(json_array_length(facts_json, '$.allocation_sites')), 0) FROM cpp_type_facts",
            [],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count)?)
    }

    /// Source regions for unmodeled allocation/initialization invocations.
    pub fn cpp_allocation_sites(
        &self,
        canceled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Vec<(FileId, types::cpp::CppAllocationSite)>> {
        anyhow::ensure!(!canceled(), "allocation region read canceled");
        let conn = self.lock_read();
        let mut statement = conn.prepare("SELECT file_id, item.value FROM cpp_type_facts, json_each(facts_json, '$.allocation_sites') AS item")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, FileId>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            anyhow::ensure!(!canceled(), "allocation region read canceled");
            let (file, json) = row?;
            Ok((file, serde_json::from_str(&json)?))
        })
        .collect()
    }

    /// Anonymous body locations and containment for query continuation. These
    /// facts do not establish invocation and do not load all receiver types.
    pub fn cpp_lambda_captures(
        &self,
    ) -> anyhow::Result<Vec<(FileId, types::cpp::CppLambdaCapture)>> {
        let conn = self.lock_read();
        let mut statement = conn.prepare("SELECT file_id, item.value FROM cpp_type_facts, json_each(facts_json, '$.lambda_captures') AS item")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, FileId>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (file, json) = row?;
            Ok((file, serde_json::from_str(&json)?))
        })
        .collect()
    }
    /// Precise failed type lookups from the project resolution pass. The
    /// enclosing index Artifact controls publication; query readers never write.
    pub fn cpp_type_lookup_failures(
        &self,
        canceled: &dyn Fn() -> bool,
    ) -> anyhow::Result<HashMap<types::ReferenceId, types::cpp::CppTypeLookupFailure>> {
        anyhow::ensure!(!canceled(), "type lookup diagnostic read canceled");
        let conn = self.lock_read();
        let mut statement = conn.prepare(
            "SELECT reference_id,
                    json_set(failure_json, '$.related_declarations', json('[]')),
                    json_extract(failure_json, '$.related_declarations')
             FROM \"references\"
             WHERE resolved_symbol_id IS NULL AND failure_json IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, types::ReferenceId>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        // Many calls stop on the same declaration inventory. Decode each
        // repeated list once, while retaining every call's distinct diagnostic.
        // This per-read memo bounds cached string/vector payload, not map overhead.
        const MEMO_BYTES: usize = 8 * 1024 * 1024;
        let mut lists = HashMap::<String, Vec<(FileId, types::TextRange)>>::new();
        let mut memo_bytes = 0;
        let mut failures = HashMap::new();
        for row in rows {
            anyhow::ensure!(!canceled(), "type lookup diagnostic read canceled");
            let (reference, json, related) = row?;
            let mut failure: types::cpp::CppTypeLookupFailure = serde_json::from_str(&json)?;
            failure.related_declarations = if let Some(list) = lists.get(&related) {
                list.clone()
            } else {
                let list: Vec<(FileId, types::TextRange)> = serde_json::from_str(&related)?;
                let bytes = related.capacity().saturating_add(
                    list.capacity()
                        .saturating_mul(std::mem::size_of::<(FileId, types::TextRange)>()),
                );
                if bytes <= MEMO_BYTES {
                    if memo_bytes + bytes > MEMO_BYTES {
                        lists.clear();
                        memo_bytes = 0;
                    }
                    memo_bytes += bytes;
                    lists.insert(related, list.clone());
                }
                list
            };
            failures.insert(reference, failure);
        }
        anyhow::ensure!(!canceled(), "type lookup diagnostic read canceled");
        Ok(failures)
    }

    /// Replace the previous failed attempt, including clearing a formerly
    /// precise cause when the current attempt cannot establish it.
    pub fn batch_update_cpp_type_lookup_failures(
        &self,
        failures: &[(types::ReferenceId, Option<types::cpp::CppTypeLookupFailure>)],
    ) -> anyhow::Result<()> {
        if failures.is_empty() {
            return Ok(());
        }
        self.with_transaction(|tx| {
            let mut statement = tx.prepare(
                "UPDATE \"references\" SET failure_json = ?2
                 WHERE reference_id = ?1 AND resolved_symbol_id IS NULL
                   AND (failure_json IS NOT NULL OR ?2 IS NOT NULL)",
            )?;
            for (reference, failure) in failures {
                let json = failure.as_ref().map(serde_json::to_string).transpose()?;
                statement.execute(rusqlite::params![reference, json])?;
            }
            Ok(())
        })
    }

    /// Source-located local lookup limits for read-only query diagnostics.
    /// Avoid loading unrelated receiver and callable facts into the graph cache.
    pub fn cpp_local_lookup_limits(&self) -> anyhow::Result<HashMap<FileId, Vec<CppLookupLimit>>> {
        let conn = self.lock_read();
        let mut statement = conn.prepare("SELECT file_id, item.value FROM cpp_type_facts, json_each(facts_json, '$.lookup_limits') AS item WHERE json_extract(item.value, '$.block_range') IS NOT NULL")?;
        let values = statement.query_map([], |row| {
            Ok((row.get::<_, FileId>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut limits = HashMap::<_, Vec<_>>::new();
        for value in values {
            let (file_id, json) = value?;
            limits
                .entry(file_id)
                .or_default()
                .push(serde_json::from_str(&json)?);
        }
        Ok(limits)
    }

    /// Small public projection for concrete call-owner gaps, without loading
    /// every receiver/parameter fact into a graph-query cache.
    pub fn cpp_unverified_callable_scopes(&self) -> anyhow::Result<HashSet<types::SymbolId>> {
        let conn = self.lock_read();
        let mut statement = conn.prepare("SELECT item.value FROM cpp_type_facts, json_each(facts_json, '$.unverified_callable_scopes') AS item")?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        values
            .map(|value| value?.parse::<types::SymbolId>())
            .collect()
    }
    pub fn cpp_types_for_file(&self, file_id: &FileId) -> anyhow::Result<Option<CppFileTypes>> {
        let json: Option<String> = self
            .lock_read()
            .query_row(
                "SELECT facts_json FROM cpp_type_facts WHERE file_id = ?1",
                [file_id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(Into::into)
    }

    /// Loaded once for a resolution session, never per candidate/call.
    pub fn all_cpp_types(&self) -> anyhow::Result<HashMap<FileId, CppFileTypes>> {
        let conn = self.lock_read();
        let mut stmt = conn.prepare("SELECT file_id, facts_json FROM cpp_type_facts")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, FileId>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (file, json) = row?;
            Ok((file, serde_json::from_str(&json)?))
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{
        FileFacts, FileInfo, Language, SymbolId, TextRange,
        cpp::{CppBaseClass, CppCallableDeclaration, CppDeclaredType, CppRecordType, CppValueType},
    };

    #[test]
    fn lookup_diagnostic_reads_preserve_repeated_lists_and_cancellation() {
        use types::cpp::{CppTypeLookupFailure, CppTypeLookupFailureKind};
        let store = Store::open_in_memory().unwrap();
        store.init_schema().unwrap();
        let file_id = FileId::generate("diagnostics.cpp");
        let mut facts = FileFacts {
            file: FileInfo {
                file_id,
                path: "diagnostics.cpp".into(),
                language: Language::Cpp,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut expected = HashMap::new();
        for index in 1..=6u8 {
            let id = types::ReferenceId::from_bytes([index; 32]);
            let range = TextRange {
                start_byte: u32::from(index) * 10,
                end_byte: u32::from(index) * 10 + 4,
                ..Default::default()
            };
            facts.references.push(types::ReferenceUse {
                id,
                file_id,
                source_symbol: None,
                scope_id: None,
                kind: types::ReferenceKind::Call,
                text: "call".into(),
                name: "call".into(),
                receiver: None,
                arity: Some(0),
                range,
                binding_id: None,
                resolved: None,
            });
            let first = (
                FileId::generate("first.h"),
                TextRange {
                    start_byte: 2,
                    end_byte: 6,
                    ..Default::default()
                },
            );
            let second = (
                FileId::generate("second.h"),
                TextRange {
                    start_byte: 7,
                    end_byte: 11,
                    ..Default::default()
                },
            );
            expected.insert(
                id,
                CppTypeLookupFailure {
                    kind: CppTypeLookupFailureKind::NameLookupRestricted,
                    name: format!("call_{index}"),
                    scope: "scope".into(),
                    file_id,
                    range,
                    related_declarations: if index % 2 == 0 {
                        vec![first, second, first]
                    } else {
                        vec![second]
                    },
                },
            );
        }
        store.insert_file_facts(&facts).unwrap();
        let updates: Vec<_> = expected
            .iter()
            .map(|(id, failure)| (*id, Some(failure.clone())))
            .collect();
        store
            .batch_update_cpp_type_lookup_failures(&updates)
            .unwrap();
        assert_eq!(store.cpp_type_lookup_failures(&|| false).unwrap(), expected);
        assert!(store.cpp_type_lookup_failures(&|| true).is_err());
        let checks = std::cell::Cell::new(0);
        assert!(
            store
                .cpp_type_lookup_failures(&|| {
                    checks.set(checks.get() + 1);
                    checks.get() >= 4
                })
                .is_err()
        );
        assert_eq!(store.cpp_type_lookup_failures(&|| false).unwrap(), expected);
        let removed = facts.references[0].id;
        store
            .batch_update_cpp_type_lookup_failures(&[(removed, None)])
            .unwrap();
        expected.remove(&removed);
        assert_eq!(store.cpp_type_lookup_failures(&|| false).unwrap(), expected);
        store.lock().execute("UPDATE \"references\" SET failure_json = json_remove(failure_json, '$.related_declarations') WHERE failure_json IS NOT NULL", []).unwrap();
        assert!(
            store.cpp_type_lookup_failures(&|| false).is_err(),
            "missing required source context must not become an empty list"
        );
    }

    #[test]
    fn cpp_type_facts_follow_atomic_file_replacement_and_deletion() {
        let store = Store::open_in_memory().unwrap();
        store.init_schema().unwrap();
        let file_id = FileId::generate("types.cpp");
        let unverified_owner =
            SymbolId::generate(&file_id, "cpp", "api::recovered", "function", None);
        let cpp = CppFileTypes {
            allocation_sites: vec![types::cpp::CppAllocationSite {
                range: TextRange {
                    start_byte: 10,
                    end_byte: 20,
                    ..TextRange::default()
                },
                source_symbol: Some(unverified_owner),
            }],
            lookup_limits: vec![types::cpp::CppLookupLimit {
                scope: "api".into(),
                name: Some("Imported".into()),
                declaration_range: TextRange::default(),
                block_range: Some(TextRange {
                    end_byte: 40,
                    ..TextRange::default()
                }),
            }],
            unverified_callable_scopes: vec![unverified_owner],
            records: vec![CppRecordType {
                symbol_id: SymbolId::generate(&file_id, "cpp", "api::Device", "class", None),
                is_definition: true,
                bases: Some(vec![CppBaseClass {
                    declared_type: CppDeclaredType {
                        name: "Base".into(),
                        template_arguments: vec![],
                        pointer: false,
                        reference: false,
                        const_: false,
                        volatile: false,
                    },
                    virtual_: false,
                    range: TextRange::default(),
                }]),
                identity_supported: true,
                lookup_supported: true,
                template_parameters: None,
            }],
            callables: vec![CppCallableDeclaration {
                symbol_id: SymbolId::generate(&file_id, "cpp", "api::Device::run", "method", None),
                template_parameters: None,
                parameter_types: vec!["const api::Value&".into()],
                parameter_declared_types: vec![Some(CppDeclaredType {
                    name: "api::Value".into(),
                    template_arguments: vec![],
                    pointer: false,
                    reference: true,
                    const_: true,
                    volatile: false,
                })],
                minimum_arity: 1,
                qualifiers: "const".into(),
                is_virtual: false,
                internal_linkage: false,
                return_type: None,
            }],
            values: vec![CppValueType {
                binding_id: None,
                symbol_id: None,
                capture_required: true,
                mutable_: false,
                bit_field: false,
                declared_type: Some(CppDeclaredType {
                    name: "api::Device".into(),
                    template_arguments: Vec::new(),
                    pointer: true,
                    reference: false,
                    const_: true,
                    volatile: false,
                }),
                initializer_call: None,
                lookup_scope: "app".into(),
                declaration_range: TextRange::default(),
            }],
            aliases: vec![types::cpp::CppTypeAlias {
                symbol_id: unverified_owner,
                target: Some(types::cpp::CppDeclaredType {
                    name: "int".into(),
                    template_arguments: vec![],
                    pointer: false,
                    reference: false,
                    const_: false,
                    volatile: false,
                }),
                target_range: TextRange::default(),
            }],
            lambda_captures: vec![types::cpp::CppLambdaCapture {
                symbol_id: Some(unverified_owner),
                enclosing_symbol: None,
                binding_id: None,
                binding_const: false,
                parameter_count: Some(0),
                range: TextRange::default(),
                body_range: TextRange::default(),
                default: Some(types::cpp::CppCaptureKind::Reference),
                captures: Some(vec![("value".into(), types::cpp::CppCaptureKind::Copy)]),
                mutable_: false,
            }],
            ..Default::default()
        };
        let mut facts = FileFacts {
            file: FileInfo {
                file_id,
                path: "types.cpp".into(),
                language: Language::Cpp,
                ..Default::default()
            },
            cpp_types: Some(cpp.clone()),
            ..Default::default()
        };
        store.insert_file_facts(&facts).unwrap();
        assert_eq!(store.cpp_allocation_site_count().unwrap(), 1);
        assert_eq!(
            store.cpp_allocation_sites(&|| false).unwrap(),
            vec![(file_id, cpp.allocation_sites[0].clone())]
        );
        assert!(store.cpp_allocation_sites(&|| true).is_err());
        assert_eq!(
            store.cpp_lambda_captures().unwrap(),
            vec![(file_id, cpp.lambda_captures[0].clone())]
        );
        assert_eq!(
            store.cpp_types_for_file(&file_id).unwrap(),
            Some(cpp.clone())
        );
        assert_eq!(store.all_cpp_types().unwrap()[&file_id], cpp);
        assert_eq!(
            store.cpp_local_lookup_limits().unwrap()[&file_id],
            cpp.lookup_limits
        );
        assert_eq!(
            store.cpp_unverified_callable_scopes().unwrap(),
            HashSet::from([unverified_owner])
        );
        facts.cpp_types = Some(CppFileTypes::default());
        store.replace_file_facts(&file_id, &facts).unwrap();
        assert_eq!(store.cpp_allocation_site_count().unwrap(), 0);
        assert!(store.cpp_allocation_sites(&|| false).unwrap().is_empty());
        assert_eq!(store.cpp_types_for_file(&file_id).unwrap(), facts.cpp_types);
        assert!(store.cpp_unverified_callable_scopes().unwrap().is_empty());
        assert!(store.cpp_local_lookup_limits().unwrap().is_empty());
        store.delete_file_data(&file_id).unwrap();
        assert!(store.all_cpp_types().unwrap().is_empty());
    }
}
