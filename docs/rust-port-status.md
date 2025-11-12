# Rust port status

The `crates/postcss` crate now hosts a native Rust implementation that mirrors
PostCSS 8.4.31. Core pieces that have shipped include:

- a shared AST with node types (`Root`, `Rule`, `AtRule`, `Declaration`,
  `Comment`, and `Document`) that preserve raw formatting and traversal helpers;
- the tokenizer, parser, and stringifier translated from the JavaScript
  reference implementation;
- input, source map, and warning utilities that reproduce PostCSS diagnostics;
- a processor pipeline capable of running Rust plugins with visitor hooks,
  lazy execution (`LazyResult`), and source-map aware result generation;
- parity helpers such as `postcss::list` splitting utilities and terminal
  highlighting.

Recent parity additions include a Rust-native `postcss::plugin` builder,
`postcss()` convenience constructors that mirror the JavaScript entry point,
and helper functions (`root()`, `rule()`, `decl()`, etc.) that match the
factory exports shipped with PostCSS 8.4.31.

The processor now also exposes the zero-plugin `NoWorkResult` fast path so
`Processor::process` matches JavaScript behaviour when no plugins or custom
syntaxes are configured.

Ongoing parity work is tracked in `PLAN.md` and focuses on rounding out the
remaining PostCSS APIs (JSON hydration, plugin factories, and exhaustive test
fixtures) so the crate can act as a drop-in replacement across ecosystems.
