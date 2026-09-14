# Local grammar changes

This is the published `tree-sitter-cpp` 0.23.4 crate (MIT), with a bounded grammar
correction for preprocessing control lines inside enumerator and initializer
lists. `src/grammar.json` is the canonical grammar source distributed with the
published crate; it is retained here with the correction.
The crate records upstream commit `f41e1a044c8a84ea9fa8577fdd2eab92ec96de02`; the bundled
LICENSE is read from that exact commit.

Control lines retain their existing named nodes and do not consume a list item
or a comma. Enumerator conditional branches keep the existing `preproc_if`,
`preproc_ifdef`, and alternative nodes. The initializer/block ambiguity is
resolved by the existing GLR mechanism, with an explicit conflict between
`_block_item` and `initializer_list`. This does not expand macros, resolve
includes, or evaluate additional preprocessor conditions.

Regenerate `src/parser.c` and `src/node-types.json` with tree-sitter CLI 0.26.9:

```sh
tree-sitter generate src/grammar.json --abi 14
```

`src/scanner.c`, Rust bindings and query files retain the upstream contents.
Both Atlas extraction and CodeServer Surface use this single local grammar.
Regression coverage lives in extraction's `cpp_preprocessor_declarations` test
and the existing C++ extraction/resolution tests.

Upstream crate checksum: `df2196ea9d47b4ab4a31b9297eaa5a5d19a0b121dceb9f118f6790ad0ab94743`.
Upstream source checksums:

- `src/grammar.json`: `c0a93751d708440bb2c1929d738a5b03c4b835f61e5aeb580d0af269faf68f8d`
- `src/parser.c`: `2a35a43b4af6c9f7b69624ac00c2c50808912591450dc79c05dea03ac1bae814`
- `src/scanner.c`: `cf60387d290271f4d2fb558d0569b2b9ef879cb72206a9df5cc73b0b1a4b20ff`
