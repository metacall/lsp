//! Helpers shared by the integration tests. Each test binary compiles its own
//! copy, so a helper one binary does not use must not warn.
#![allow(dead_code, reason = "not every test binary uses every helper")]

use std::borrow::Cow;
use std::path::Path;

use meta_call_lsp::convert;
use meta_call_lsp::index::SourceText;
use meta_call_lsp::types::DocUri;

/// Disk-only source lookup.
pub struct DiskSources;

impl SourceText for DiskSources {
    fn source(&self, path: &Path) -> Option<Cow<'_, str>> {
        std::fs::read_to_string(path).ok().map(Cow::Owned)
    }
}

pub fn doc_uri(value: &str) -> DocUri {
    DocUri::try_from(value).expect("document URI")
}

pub fn uri_of(path: &Path) -> DocUri {
    DocUri::try_from(convert::path_to_uri(path).expect("uri").as_str()).expect("document URI")
}
