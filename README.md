# MetaCall LSP

Polyglot language server for MetaCall workspaces. It gives cross-language navigation, hover, and completion across Python, JavaScript, TypeScript, and other supported languages in one server.

## What it does

- Serves one workspace from one process over LSP and stdio.
- Answers from a static index built by `meta-ast`. It never runs user code.
- Tracks open buffers so unsaved edits stay visible.
- Reindexes in the background and swaps snapshots without blocking queries.
- Coexists with native language servers. It adds cross-language queries only.

## What it does not do now

- No full type inference.
- No cross-language rename.
- No code execution.
- No edits to user sources.

## Clients

Any LSP client works: Neovim, Helix, Emacs. Point the client at the server binary over stdio. Thin VSCode and Zed clients are planned; no client code ships yet.

## Layout

```text
src/        server crate
tests/      protocol tests and fixtures
```

## Roadmap

- Phase 1: single-language correctness. Sync, symbols, hover, definition, diagnostics.
- Phase 2: polyglot queries. Cross-language definition, references, workspace symbols, ranked completion.
- Phase 3: runtime hover data, semantic tokens, stub emit.
- Packaging: thin VSCode and Zed clients, per-platform release binaries.

## Development

Toolchain: Rust 1.94.0, pinned in `rust-toolchain.toml`.

```bash
cargo build --all-features
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

CI runs the same four commands on push to `main` and on pull requests.

## License

Apache-2.0. See `LICENSE`.
