//! The library's combined error: what can go wrong on a path that loads,
//! analyses and writes.

use std::path::PathBuf;

use crate::load::LoadError;
use crate::series::ParamsError;

/// Why a report, a trend or a cache write failed.
///
/// Like [`LoadError`], no variant repeats its source in its own message:
/// callers print the chain (`anyhow`'s `{:?}`, or
/// [`crate::load::error_chain`] for a log line), and spelling the cause twice
/// is what that convention exists to prevent. `Display` alone is therefore
/// the outermost step only.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    /// The recording could not be read, or its header is unusable.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// The analysis parameters are out of range.
    #[error(transparent)]
    Params(#[from] ParamsError),
    /// A file or directory could not be read or written.
    #[error("{action} {path}")]
    Io {
        /// What was being attempted, e.g. `"writing"`.
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// A document could not be serialized.
    #[error("encoding {path} as JSON")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl AnalyzeError {
    /// An [`AnalyzeError::Io`] for `path`.
    pub(crate) fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}
