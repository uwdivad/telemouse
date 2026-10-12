//! A recording's identity for cache validation.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::LoadError;

/// Bytes hashed at each of the three sample points.
const SIGNATURE_SAMPLE_BYTES: u64 = 4 * 1024;

/// A filesystem timestamp, split so it serializes exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Timestamp {
    pub secs: u64,
    pub nanos: u32,
}

/// A recording's identity for cache validation: size, timestamps, and content
/// samples. Used by the listing cache here and by the per-session report cache
/// in [`crate::trend`], which needs the same question answered — *is this
/// cached artifact still about this file?* — and cannot answer it with mtime
/// alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileSignature {
    pub len: u64,
    pub modified: Option<Timestamp>,
    pub created: Option<Timestamp>,
    /// FNV-1a over small samples at the front, middle and tail. Size and mtime
    /// are the primary invalidators; this also catches replacement files on
    /// filesystems with coarse timestamp precision.
    pub sample_hash: u64,
}

fn timestamp(value: std::io::Result<SystemTime>) -> Option<Timestamp> {
    let d = value.ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Timestamp {
        secs: d.as_secs(),
        nanos: d.subsec_nanos(),
    })
}

fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, Option<Timestamp>, Option<Timestamp>) {
    (
        metadata.len(),
        timestamp(metadata.modified()),
        timestamp(metadata.created()),
    )
}

/// A cheap but change-sensitive recording signature. Sampling three locations
/// keeps a cache hit O(1) in recording size while guarding against same-size
/// replacement files whose timestamps were rounded by the filesystem.
///
/// `Ok(None)` means the file changed *while* it was being sampled (capture is
/// still appending): the current read may still be useful, but nothing about
/// it may be cached.
pub fn file_signature(path: &Path) -> Result<Option<FileSignature>, LoadError> {
    let io = |source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(io)?;
    let before = file.metadata().map_err(io)?;
    let (len, modified, created) = metadata_identity(&before);

    let sample_len = len.min(SIGNATURE_SAMPLE_BYTES);
    let middle = len
        .saturating_div(2)
        .saturating_sub(sample_len.saturating_div(2));
    let tail = len.saturating_sub(sample_len);
    let mut offsets = [0, middle, tail];
    offsets.sort_unstable();

    let mut hash = 0xcbf29ce484222325u64;
    let mut previous = None;
    let mut buf = vec![0u8; sample_len as usize];
    for offset in offsets {
        if previous == Some(offset) {
            continue;
        }
        previous = Some(offset);
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(&mut buf))
            .map_err(io)?;
        for byte in offset.to_le_bytes().into_iter().chain(buf.iter().copied()) {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }

    // Never cache a view taken while capture was extending or replacing the
    // file. The current scan may still be useful, but the next invocation must
    // inspect it again.
    let after = file.metadata().map_err(io)?;
    if metadata_identity(&after) != (len, modified, created) {
        return Ok(None);
    }

    Ok(Some(FileSignature {
        len,
        modified,
        created,
        sample_hash: hash,
    }))
}
