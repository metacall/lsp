//! MetaCall polyglot language server.
pub mod buffers;
pub mod cancel;
pub mod convert;
pub mod error;
pub mod handlers;
pub mod index;
pub mod position;
pub mod reindex;
pub mod server;
pub mod shards;
pub mod types;

#[cfg(test)]
pub(crate) mod testutil {
    use crate::types::DocUri;

    pub(crate) fn doc_uri(value: &str) -> DocUri {
        DocUri::try_from(value).expect("document URI")
    }
}
