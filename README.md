# MetaCall LSP

Polyglot language server for MetaCall workspaces. It gives cross-language navigation, hover, and completion across Python, JavaScript, TypeScript, and other supported languages in one server.

## What it does

- Serves one workspace from one process over LSP and stdio.
- Answers from a static index built by `meta-ast`. It never runs user code.
- Tracks open buffers so unsaved edits stay visible.
- Coexists with native language servers. It adds cross-language queries only.

## What it does not do now

- No full type inference.
- No cross-language rename in v1.
- No code execution.
- No edits to user sources.

## Clients

Primary: VSCode and Zed.

Any LSP client works: Neovim, Helix, Emacs. Point the client at the server binary over stdio.

## Layout

```text
src/        server
clients/    thin editors clients
docs/       user docs
```

## Roadmap

- Phase 1: single-language correctness. Sync, symbols, hover, definition, diagnostics.
- Phase 2: polyglot queries. Cross-language definition, references, workspace symbols, ranked completion.
- Phase 3: Runtime hover data, semantic tokens, stub emit.
- Phase 4: Code execution, cross-language rename.

## Development

Toolchain: Rust 1.94.0.

```bash
cargo build --all-features
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

## License

Apache-2.0. See `LICENSE`.
