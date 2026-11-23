//! Discovery and the reads of one transcript file: blocking I/O, run on the blocking pool by
//! [`crate::Ingest`] (#3, "Ingest contract").

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bosun_core::WindowKey;

use crate::layout::{FileKind, Provider};
use crate::line::{Batch, LineParser, parse_lines_after};
use crate::meter::Gauge;
use crate::record::Source;

/// How deep discovery walks below the provider's directory. Claude Code's deepest transcript
/// is four levels down (`<project>/<session>/subagents/agent-<id>.jsonl`).
const MAX_DEPTH: usize = 6;

/// A transcript file found by discovery, or examined by a tailer.
#[derive(Debug, Clone)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    /// The file id: the source file's key.
    pub(crate) key: String,
    pub(crate) window: WindowKey,
    pub(crate) size: u64,
    /// When the file was last written, where the platform says.
    pub(crate) modified: Option<SystemTime>,
    /// The window's meta file, when there is one.
    pub(crate) meta: Option<PathBuf>,
}

/// Every transcript under `root`, sorted by path, and the files that could not be examined.
/// A missing `root` holds nothing: the provider has not written there yet.
pub(crate) fn discover<P: Provider>(
    provider: &P,
    root: &Path,
) -> (Vec<Found>, Vec<(PathBuf, io::Error)>) {
    let mut transcripts = Vec::new();
    let mut failed = Vec::new();
    walk(provider, root, 0, &mut |path| {
        if provider
            .locate(path)
            .is_some_and(|located| located.kind == FileKind::Transcript)
        {
            transcripts.push(path.to_owned());
        }
    })
    .unwrap_or_else(|(path, e)| {
        if e.kind() != io::ErrorKind::NotFound || path != root {
            failed.push((path, e));
        }
    });

    let mut found = Vec::new();
    for path in transcripts {
        match examine(provider, &path, None) {
            Ok(Some(file)) => found.push(file),
            // Deleted since it was listed: its rows stay, as for any deleted file.
            Ok(None) => {}
            Err(e) => failed.push((path, e)),
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    (found, failed)
}

/// The transcript at `path` as it is now, or `None` when it is gone or is not a transcript.
/// On Windows the file id is read through a handle; `open` counts it while it is held.
pub(crate) fn examine<P: Provider>(
    provider: &P,
    path: &Path,
    open: Option<&Arc<Gauge>>,
) -> io::Result<Option<Found>> {
    let Some(located) = provider.locate(path) else {
        return Ok(None);
    };
    if located.kind != FileKind::Transcript {
        return Ok(None);
    }
    if cfg!(mutant_ingest_skip_subagents) && located.window.agent_id.is_some() {
        // Mutant: sub-agent transcripts are not discovered.
        return Ok(None);
    }
    let examined = std::fs::metadata(path).and_then(|meta| {
        let _held = open.map(|open| Gauge::hold(open, 1));
        Ok((meta, file_key(path)?))
    });
    let (meta, key) = match examined {
        Ok(examined) => examined,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    Ok(Some(Found {
        meta: provider.meta_path(path).filter(|meta| meta.is_file()),
        path: path.to_owned(),
        key,
        window: located.window,
        size: meta.len(),
        modified: meta.modified().ok(),
    }))
}

/// Whether `path` is where discovery would look: under `root`, no deeper than [`MAX_DEPTH`],
/// and only through directories the provider descends into. A watch event outside that is
/// ignored, as discovery would ignore the file.
pub(crate) fn within<P: Provider>(provider: &P, root: &Path, path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<_> = rest.components().collect();
    let Some((_, dirs)) = parts.split_last() else {
        return false;
    };
    dirs.len() < MAX_DEPTH
        && dirs.iter().all(|dir| match dir {
            std::path::Component::Normal(name) => {
                name.to_str().is_some_and(|name| provider.descend(name))
            }
            _ => false,
        })
}

/// Calls `visit` for every file under `dir`, down to [`MAX_DEPTH`]. Unreadable subdirectories
/// are skipped; only `dir` itself failing is an error.
fn walk<P: Provider>(
    provider: &P,
    dir: &Path,
    depth: usize,
    visit: &mut dyn FnMut(&Path),
) -> Result<(), (PathBuf, io::Error)> {
    let entries = std::fs::read_dir(dir).map_err(|e| (dir.to_owned(), e))?;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            let name = entry.file_name();
            if depth + 1 < MAX_DEPTH && name.to_str().is_some_and(|name| provider.descend(name)) {
                let _ = walk(provider, &path, depth + 1, visit);
            }
        } else if kind.is_file() {
            visit(&path);
        }
    }
    Ok(())
}

/// The key of a file: its file id (device and inode on Unix, volume serial and file index on
/// Windows), so that a renamed file keeps its key and a recreated one gets a new one.
pub(crate) fn file_key(path: &Path) -> io::Result<String> {
    Ok(match file_id::get_file_id(path)? {
        file_id::FileId::Inode {
            device_id,
            inode_number,
        } => format!("inode:{device_id:x}:{inode_number:x}"),
        file_id::FileId::LowRes {
            volume_serial_number,
            file_index,
        } => format!("index:{volume_serial_number:x}:{file_index:x}"),
        file_id::FileId::HighRes {
            volume_serial_number,
            file_id,
        } => format!("id:{volume_serial_number:x}:{file_id:x}"),
    })
}

/// The stored form of a line's hash: its length and its 64-bit FNV-1a, so that the line can
/// be found again by reading back that many bytes before the offset.
pub(crate) fn line_hash(line: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in line {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{}:{hash:016x}", line.len())
}

/// Whether the line that ends at `offset` in `file` still hashes to `hash`: the file grew,
/// rather than being rewritten.
pub(crate) fn hash_matches(file: &mut File, offset: u64, hash: &str) -> io::Result<bool> {
    let Some(len) = hash
        .split_once(':')
        .and_then(|(len, _)| len.parse::<u64>().ok())
    else {
        return Ok(false);
    };
    let Some(start) = offset.checked_sub(len) else {
        return Ok(false);
    };
    file.seek(SeekFrom::Start(start))?;
    let mut line = Vec::new();
    file.take(len).read_to_end(&mut line)?;
    Ok(line_hash(&line) == hash)
}

/// The complete lines of one read and the hash of the last of them.
#[derive(Debug)]
pub(crate) struct Chunk {
    pub(crate) batch: Batch,
    pub(crate) last_hash: Option<String>,
    /// The read filled its limit: there may be more bytes past it.
    pub(crate) full: bool,
    /// Bytes read into memory, complete lines or not.
    pub(crate) read: u64,
}

/// Reads up to `limit` bytes of `file` from `offset` and parses every complete line of them;
/// `last_ts_ms` is the last timestamp of the lines before `offset`.
pub(crate) fn read_chunk<P: LineParser>(
    parser: &P,
    source: &Source,
    file: &mut File,
    (offset, last_ts_ms): (u64, Option<i64>),
    limit: u64,
) -> io::Result<Chunk> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    let read = bytes.len() as u64;
    let full = read >= limit;
    if cfg!(mutant_ingest_parse_partial) && !bytes.is_empty() && !bytes.ends_with(b"\n") {
        // Mutant: a last line still being written is parsed as if it were complete.
        bytes.push(b'\n');
    }
    let batch = parse_lines_after(parser, source, offset, &bytes, last_ts_ms);
    let last_hash = batch.lines.last().map(|line| {
        let start = usize::try_from(line.offset - offset).unwrap_or(usize::MAX);
        let end = usize::try_from(line.end - offset).unwrap_or(usize::MAX);
        line_hash(&bytes[start..end])
    });
    Ok(Chunk {
        batch,
        last_hash,
        full,
        read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_hash_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.jsonl");
        std::fs::write(&path, b"first\nsecond\n").unwrap();
        let hash = line_hash(b"second\n");
        assert!(hash.starts_with("7:"));
        let mut file = File::open(&path).unwrap();
        assert!(hash_matches(&mut file, 13, &hash).unwrap());
        assert!(!hash_matches(&mut file, 6, &hash).unwrap());
        assert!(!hash_matches(&mut file, 3, &hash).unwrap());
        assert!(!hash_matches(&mut file, 13, "garbage").unwrap());
    }

    #[test]
    fn a_renamed_file_keeps_its_key_and_a_recreated_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"x").unwrap();
        let key = file_key(&a).unwrap();
        std::fs::rename(&a, &b).unwrap();
        assert_eq!(file_key(&b).unwrap(), key);
        std::fs::write(&a, b"x").unwrap();
        assert_ne!(file_key(&a).unwrap(), key);
    }
}
