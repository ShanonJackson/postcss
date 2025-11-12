# PostCSS 8.4.31 Rust Parity Plan

This document translates the previously agreed-upon parity outline into an implementation plan for `crates/postcss`, delivering a drop-in replacement for the JavaScript distribution while preserving every observable behavior.

## 1. Repository Layout & Exports
- Create a new Rust crate at `crates/postcss` that mirrors the JS `lib/` module structure (`postcss.rs`, `processor.rs`, `lazy_result.rs`, etc.).
- Re-export constructors and helper functions from a crate root `mod postcss` so Rust callers observe the same surface as `lib/postcss.js`.
- Maintain version strings and helper APIs (`plugin`, node constructors) exactly as 8.4.31.

## 2. AST Modeling
- Implement a `Node` base struct with raw metadata, source tracking, and mutation flags equivalent to JS symbol properties (`isClean`, `my`).
- Layer a `Container` trait/struct handling child management, normalization, and traversal (`walk`, `walk_decls`, `walk_rules`, etc.).
- Specialize `Root`, `Document`, `Rule`, `AtRule`, `Declaration`, and `Comment` with matching defaults, getters/setters, and serialization behavior.
- Provide cloning, parent references, and dirty/clean lifecycle hooks using `Rc<RefCell<_>>` or similar to emulate JS mutability.

## 3. Parsing Subsystem
- Port the tokenizer from `lib/tokenize.js` with identical state machines for escapes, brackets, and URL handling.
- Reproduce `lib/parser.js` logic for at-rule parsing, declaration detection, rule nesting, and error reporting, including Safe Parser fallbacks.
- Offer a top-level `parse` function that constructs `Input`, attaches diagnostics, and returns hydrated AST roots.

## 4. Input & Source Maps
- Implement `Input` with BOM stripping, path resolution, previous map detection, and error positioning as in `lib/input.js`.
- Port `PreviousMap` to handle inline maps, file discovery, and lazy consumer instantiation.
- Recreate `MapGenerator` for stringifying results with annotations, inline source maps, and content embedding.
- Support JSON hydration (`from_json`) to rebuild `Result`/`Warning` objects from serialized output.

## 5. Stringification
- Implement `Stringifier` mirroring `lib/stringify.js` with raw inference, indentation logic, and block/document handling.
- Provide a crate-level `stringify` function delegating to node-specific `to_string` implementations while respecting raw overrides.

## 6. Processing Pipeline
- Port `Processor` normalization of plugin inputs (functions, objects with `postcssPlugin`, nested arrays) and option handling.
- Implement `LazyResult` with synchronous/asynchronous execution parity, visitor event ordering, error propagation, and map generation hooks.
- Ensure zero-plugin pipelines reuse the same `LazyResult` fast path used by JavaScript, avoiding API deviations while still lazily parsing CSS when needed.
- Rebuild `Result`, `Warning`, `Declaration#raws`, helper injection (`result.root`, `result.css`, etc.), and one-time warning utilities.

## 7. Ancillary Utilities
- Recreate list helpers, terminal highlighting (pico-colors equivalents), ID generators (NanoID), and other exported utilities referenced by browser shims.
- Ensure environment differences (path separators, URL resolution) match Node behavior across Mac/Linux/Windows.

## 8. Testing & Verification
- Build integration tests comparing Rust outputs to the JS reference by invoking Node fixtures and asserting byte-for-byte equality of ASTs, CSS, warnings, and source maps.
- Port critical JS tests covering async plugins, error messages, traversal order, and source map generation.
- Add property/fuzz tests for tokenizer/parser/stringifier round-trips to detect divergence early.

## 9. Risks & Mitigations
- Borrow checker constraints: design interior-mutability wrappers and parent/child management carefully to avoid deadlocks.
- Async interoperability: choose Future/`async_trait` implementations that preserve JS-like execution semantics.
- Source map fidelity: validate Rust `sourcemap` bindings produce identical outputs; introduce shims if necessary.
- Terminal highlighting differences: ensure ANSI color outputs align with pico-colors expectations.

## 10. Delivery Criteria
- `crates/postcss` builds successfully and exposes a public API mirroring PostCSS 8.4.31.
- For any input processed by the JS version, the Rust crate produces identical AST, CSS output, warnings, and source maps.
- All integration tests comparing JS and Rust outputs pass on Mac, Linux, and Windows environments.
