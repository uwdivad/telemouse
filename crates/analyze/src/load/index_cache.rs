//! The listing's disposable metadata cache, `.telemouse-analyze-index-v1.json`
//! in the recordings directory.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use super::{FileSignature, SessionIndexEntry};

/// Versioned, disposable metadata cache used by `scan_dir`. It intentionally
/// does not end in `.jsonl`, so it can live beside recordings without ever
/// being mistaken for one.
pub(super) const INDEX_CACHE_FILE: &str = ".telemouse-analyze-index-v1.json";
/// The version field inside the file. `2`: entries are scanned with the
/// loader's own line classifier and only after the session header validated,
/// so a version-1 entry may list a recording `report` refuses, or count a bad
/// marker line as good. A mismatch rebuilds the cache once; the file name
/// stays, so nothing that knows where the cache lives has to change.
pub(super) const INDEX_CACHE_VERSION: u32 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct CachedSession {
    /// Path relative to the directory containing the cache: the file's stable
    /// identity for directory-listing purposes.
    pub file_name: PathBuf,
    pub signature: FileSignature,
    pub entry: SessionIndexEntry,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct SessionIndexCache {
    pub version: u32,
    pub files: Vec<CachedSession>,
}

/// The cached entries by file name; empty when there is no cache, it does not
/// parse, or it is another version's.
pub(super) fn read_index_cache(dir: &Path) -> HashMap<PathBuf, CachedSession> {
    let path = dir.join(INDEX_CACHE_FILE);
    let Ok(file) = File::open(path) else {
        return HashMap::new();
    };
    let Ok(cache) = serde_json::from_reader::<_, SessionIndexCache>(BufReader::new(file)) else {
        return HashMap::new();
    };
    if cache.version != INDEX_CACHE_VERSION {
        return HashMap::new();
    }
    cache
        .files
        .into_iter()
        .map(|cached| (cached.file_name.clone(), cached))
        .collect()
}

/// The cache is only an acceleration structure, so failure to update it must
/// never turn a successful listing into an error; it is published atomically
/// so a crash or a concurrent `list` cannot expose half a JSON document.
pub(super) fn write_index_cache(dir: &Path, mut files: Vec<CachedSession>) {
    files.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    let cache = SessionIndexCache {
        version: INDEX_CACHE_VERSION,
        files,
    };
    let path = dir.join(INDEX_CACHE_FILE);
    if let Err(error) = crate::atomic::write_atomic(&path, |w| {
        serde_json::to_writer(w, &cache).map_err(std::io::Error::other)
    }) {
        tracing::debug!(path = %path.display(), %error, "could not update recording index cache");
    }
}
