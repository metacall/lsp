//! MetaCall polyglot language server binary.
fn main() -> anyhow::Result<()> {
    meta_call_lsp::server::run()
}
