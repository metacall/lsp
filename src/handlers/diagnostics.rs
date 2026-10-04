//! Pull diagnostics for one document.
use super::QueryCtx;
use crate::types::DocUri;

const CAP_DIAGNOSTICS: usize = 500;

pub fn diagnostics_for(ctx: &mut QueryCtx, uri: &DocUri) -> Vec<lsp_types::Diagnostic> {
    let Some(path) = uri.to_path() else {
        return Vec::new();
    };
    let mut items: Vec<lsp_types::Diagnostic> = ctx
        .snapshot()
        .diagnostics_for_path(&path)
        .map(|diagnostic| ctx.diagnostic(diagnostic))
        .collect();
    if items.len() > CAP_DIAGNOSTICS {
        tracing::debug!(
            total = items.len(),
            cap = CAP_DIAGNOSTICS,
            "diagnostics truncated"
        );
        items.truncate(CAP_DIAGNOSTICS);
    }
    items
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::Path;

    use super::*;
    use crate::index::{SourceText, rebuild_from_inputs};
    use crate::position::Encoding;

    struct NoSources;

    impl SourceText for NoSources {
        fn source(&self, _path: &Path) -> Option<Cow<'_, str>> {
            None
        }
    }

    #[test]
    fn untitled_uri_reports_no_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = rebuild_from_inputs(dir.path(), &[]).unwrap();
        let mut ctx = QueryCtx::new(&snapshot, &NoSources, Encoding::Utf16);

        let untitled = crate::types::DocUri::try_from("untitled:buffer.py").expect("document URI");
        assert!(diagnostics_for(&mut ctx, &untitled).is_empty());
    }
}
