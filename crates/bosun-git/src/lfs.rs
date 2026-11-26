//! Git LFS objects (#8 "Git LFS objects follow a landed commit"): which Git LFS pointers a
//! commit or a range holds, where a repository keeps its Git LFS objects, and carrying them
//! from one repository's store to another's.
//!
//! Git LFS keeps a file's content outside git's object store, in
//! `<storage>/objects/<oid[0..2]>/<oid[2..4]>/<oid>`, and a git fetch never carries it: a
//! commit fetched from the daemon's clone into the user's repository (or from the user's
//! repository into the clone) arrives with its pointers only, and a checkout that smudges them
//! asks a remote for the content. [`carry`] copies the objects between the two stores on the
//! same machine instead.
//!
//! - [`pointers`] and [`pointers_between`] read the blobs of at most [`MAX_POINTER`] bytes of a
//!   commit's tree or of a range through one `git cat-file --batch`, and keep those that
//!   [`parse`] as a pointer. A commit with no blob that small costs one `ls-tree`.
//! - [`storage`] is a repository's object directory: `lfs.storage` from its configuration when
//!   set (relative to its git common directory), else `<git common dir>/lfs`; then `objects`.
//! - [`carry`] copies each object that is in one store and missing in the other through a
//!   temporary file renamed into place, its SHA-256 checked against its name first.
//!
//! Nothing here runs git-lfs.

use std::fmt;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::Git;

/// The largest blob read as a possible pointer: Git LFS writes none larger.
pub const MAX_POINTER: u64 = 1024;

/// The first line of a pointer.
const VERSION: &str = "version https://git-lfs.github.com/spec/v1\n";

/// A Git LFS pointer: the object it names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pointer {
    /// The object's SHA-256, 64 lowercase hex digits.
    pub oid: String,
    /// The object's size in bytes.
    pub size: u64,
}

/// What [`carry`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Carried {
    /// Objects copied into the destination.
    pub copied: usize,
    /// Objects neither store had: skipped.
    pub missing: usize,
}

/// What can go wrong reading pointers or carrying objects.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A git command failed.
    Git(crate::Error),
    /// A file of a store cannot be read or written.
    Io {
        /// The file or directory.
        path: PathBuf,
        /// What failed.
        source: std::io::Error,
    },
    /// An object's content does not hash to its name: it is not copied.
    Mismatch {
        /// The object's name.
        oid: String,
        /// The SHA-256 of its content.
        actual: String,
    },
    /// Not an object name (64 lowercase hex digits).
    Oid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(e) => e.fmt(f),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Mismatch { oid, actual } => write!(
                f,
                "the Git LFS object {oid} does not hash to its name (its content is {actual})"
            ),
            Self::Oid(oid) => write!(f, "{oid:?} is not a Git LFS object name"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Git(e) => Some(e),
            Self::Io { source, .. } => Some(source),
            Self::Mismatch { .. } | Self::Oid(_) => None,
        }
    }
}

impl From<crate::Error> for Error {
    fn from(e: crate::Error) -> Self {
        Self::Git(e)
    }
}

/// Whether `oid` is 64 lowercase hex digits.
fn is_oid(oid: &str) -> bool {
    oid.len() == 64
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `blob` as a pointer: exactly the lines `version https://git-lfs.github.com/spec/v1`,
/// `oid sha256:<64 lowercase hex>` and `size <n>`, in that order, each ending in `\n`.
#[must_use]
pub fn parse(blob: &[u8]) -> Option<Pointer> {
    if blob.len() as u64 > MAX_POINTER {
        return None;
    }
    let text = std::str::from_utf8(blob).ok()?;
    let rest = text.strip_prefix(VERSION)?;
    let (oid, rest) = rest.strip_prefix("oid sha256:")?.split_once('\n')?;
    let (size, rest) = rest.strip_prefix("size ")?.split_once('\n')?;
    if !rest.is_empty()
        || !is_oid(oid)
        || size.is_empty()
        || !size.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(Pointer {
        oid: oid.to_owned(),
        size: size.parse().ok()?,
    })
}

/// The pointers of `commit`'s tree, each once, in oid order.
///
/// # Errors
///
/// A git command fails.
pub fn pointers(git: &Git, commit: &str) -> Result<Vec<Pointer>, Error> {
    let listed = git.run_bytes(&["ls-tree", "-r", "-l", "-z", commit, "--"], None)?;
    let mut small = Vec::new();
    for entry in listed.split(|&b| b == 0) {
        // `<mode> SP <type> SP <oid> SP+ <size> TAB <path>`.
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let head = String::from_utf8_lossy(&entry[..tab]);
        let mut fields = head.split_whitespace();
        let (Some(_), Some("blob"), Some(oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if size.parse::<u64>().is_ok_and(|s| s <= MAX_POINTER) {
            small.push(oid.to_owned());
        }
    }
    read(git, small)
}

/// The pointers of the blobs `to` has and `from` has not (`git rev-list --objects
/// <from>..<to>`), each once, in oid order.
///
/// # Errors
///
/// A git command fails.
pub fn pointers_between(git: &Git, from: &str, to: &str) -> Result<Vec<Pointer>, Error> {
    let range = format!("{from}..{to}");
    let listed = git.run(&["rev-list", "--objects", &range, "--"], None)?;
    let mut input = String::new();
    for line in listed.lines() {
        if let Some(oid) = line.split_whitespace().next() {
            input.push_str(oid);
            input.push('\n');
        }
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let checked = git.run(&["cat-file", "--batch-check"], Some(&input))?;
    let mut small = Vec::new();
    for line in checked.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(oid), Some("blob"), Some(size)) = (fields.next(), fields.next(), fields.next())
            && size.parse::<u64>().is_ok_and(|s| s <= MAX_POINTER)
        {
            small.push(oid.to_owned());
        }
    }
    read(git, small)
}

/// The blobs `oids` read through one `git cat-file --batch`, kept when they are pointers.
fn read(git: &Git, mut oids: Vec<String>) -> Result<Vec<Pointer>, Error> {
    oids.sort();
    oids.dedup();
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let input: String = oids.iter().flat_map(|o| [o.as_str(), "\n"]).collect();
    let out = git.run_bytes(&["cat-file", "--batch"], Some(input.as_bytes()))?;
    let mut found = Vec::new();
    let mut rest = out.as_slice();
    while !rest.is_empty() {
        let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
            return Err(git
                .unexpected("a `cat-file --batch` header without its end".to_owned())
                .into());
        };
        let header = String::from_utf8_lossy(&rest[..nl]).into_owned();
        rest = &rest[nl + 1..];
        let mut fields = header.split_whitespace();
        let (Some(_), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            // `<oid> missing`: nothing follows.
            continue;
        };
        let size: usize = size
            .parse()
            .map_err(|_| git.unexpected(format!("the `cat-file --batch` header {header:?}")))?;
        if rest.len() < size + 1 {
            return Err(git
                .unexpected(format!(
                    "a short `cat-file --batch` object after {header:?}"
                ))
                .into());
        }
        if kind == "blob"
            && let Some(pointer) = parse(&rest[..size])
        {
            found.push(pointer);
        }
        rest = &rest[size + 1..];
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// The Git LFS object directory of the repository `git` runs in: `lfs.storage` from its
/// configuration when set (relative to its git common directory), else `<git common
/// dir>/lfs`; then `objects`. It need not exist.
///
/// # Errors
///
/// A git command fails.
pub fn storage(git: &Git) -> Result<PathBuf, Error> {
    let common = git.run(
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        None,
    )?;
    let common = PathBuf::from(common.trim());
    let lfs = match git.optional(&["config", "--get", "lfs.storage"])? {
        Some(dir) => common.join(dir),
        None => common.join("lfs"),
    };
    Ok(lfs.join("objects"))
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The path of object `oid` under the object directory `dir`.
fn object_path(dir: &Path, oid: &str) -> PathBuf {
    dir.join(&oid[..2]).join(&oid[2..4]).join(oid)
}

/// Copies each of `oids` that is in the object directory `from` and missing in `to` into `to`,
/// through a temporary file there renamed into place once its SHA-256 is its name. An object
/// `from` has not either is skipped and counted.
///
/// # Errors
///
/// A name is not an oid, a file cannot be read or written, or an object does not hash to its
/// name ([`Error::Mismatch`]: nothing of it is left in `to`). Objects copied before stay.
pub fn carry<'a>(
    from: &Path,
    to: &Path,
    oids: impl IntoIterator<Item = &'a str>,
) -> Result<Carried, Error> {
    let mut carried = Carried::default();
    for oid in oids {
        if !is_oid(oid) {
            return Err(Error::Oid(oid.to_owned()));
        }
        let target = object_path(to, oid);
        if target.is_file() {
            continue;
        }
        let source = object_path(from, oid);
        let file = match std::fs::File::open(&source) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                carried.missing += 1;
                continue;
            }
            Err(source_err) => {
                return Err(Error::Io {
                    path: source,
                    source: source_err,
                });
            }
        };
        copy_checked(file, &source, &target, oid)?;
        carried.copied += 1;
    }
    Ok(carried)
}

/// Copies `file` (at `source`) to `target` through a temporary file next to it, checking its
/// SHA-256 against `oid` before the rename.
fn copy_checked(
    mut file: std::fs::File,
    source: &Path,
    target: &Path,
    oid: &str,
) -> Result<(), Error> {
    let io = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    let dir = target.parent().unwrap_or(target);
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let temp = dir.join(format!("{oid}.{}.bosun-tmp", std::process::id()));
    // A temporary file left by a process that died with this pid is stale.
    let _ = std::fs::remove_file(&temp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(io(&temp))?;
    let copied = (|| -> Result<String, Error> {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(io(source))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(io(&temp))?;
        }
        out.sync_all().map_err(io(&temp))?;
        Ok(hex(&hasher.finalize()))
    })();
    drop(out);
    let actual = match copied {
        Ok(actual) => actual,
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
    };
    // Mutant (`mutant_git_lfs_carry_unchecked`): an object is copied whatever its content.
    if !cfg!(mutant_git_lfs_carry_unchecked) && actual != oid {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::Mismatch {
            oid: oid.to_owned(),
            actual,
        });
    }
    std::fs::rename(&temp, target).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        Error::Io {
            path: target.to_owned(),
            source: e,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::NO_HOOKS;

    /// Runs `git args` in `dir` with no user or system configuration; it must succeed.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Git::new(dir)
            .command()
            .args(["-c", "commit.gpgSign=false"])
            .args(["-c", "user.name=Test"])
            .args(["-c", "user.email=test@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", NO_HOOKS)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove(crate::MARKER_VAR)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn sha256(content: &[u8]) -> String {
        hex(&Sha256::digest(content))
    }

    fn pointer_text(oid: &str, size: usize) -> String {
        format!("{VERSION}oid sha256:{oid}\nsize {size}\n")
    }

    #[test]
    fn a_pointer_is_read_and_anything_else_is_not() {
        let content = b"object content\n";
        let oid = sha256(content);
        let valid = pointer_text(&oid, content.len());
        assert_eq!(
            parse(valid.as_bytes()),
            Some(Pointer {
                oid: oid.clone(),
                size: content.len() as u64
            })
        );
        let upper = pointer_text(&oid.to_uppercase(), content.len());
        assert_eq!(parse(upper.as_bytes()), None, "an uppercase oid");
        let no_size = format!("{VERSION}oid sha256:{oid}\n");
        assert_eq!(parse(no_size.as_bytes()), None, "no size");
        let disordered = format!("{VERSION}size 15\noid sha256:{oid}\n");
        assert_eq!(parse(disordered.as_bytes()), None, "lines out of order");
        let unended = valid.trim_end();
        assert_eq!(
            parse(unended.as_bytes()),
            None,
            "a last line without its end"
        );
        assert_eq!(parse(b"ab"), None, "a 2-byte blob");

        // In a repository: the pointer committed as a plain file is found in the tree and in
        // the range that adds it, and nothing else is.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(repo.join("small.txt"), "ab").unwrap();
        std::fs::write(repo.join("big.txt"), "x".repeat(2000)).unwrap();
        std::fs::write(repo.join("upper.bin"), &upper).unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "first"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        let g = Git::new(&repo);
        assert_eq!(pointers(&g, &first).unwrap(), []);
        std::fs::create_dir(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub").join("asset.bin"), &valid).unwrap();
        std::fs::write(repo.join("copy.bin"), &valid).unwrap();
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "-m", "second"]);
        let second = git(&repo, &["rev-parse", "HEAD"]);
        let expected = vec![Pointer {
            oid: oid.clone(),
            size: content.len() as u64,
        }];
        assert_eq!(pointers(&g, &second).unwrap(), expected);
        assert_eq!(pointers_between(&g, &first, &second).unwrap(), expected);
        assert_eq!(pointers_between(&g, &second, &second).unwrap(), []);
        let common = std::fs::canonicalize(repo.join(".git")).unwrap();
        let store = std::fs::canonicalize(storage(&g).unwrap().parent().unwrap().parent().unwrap())
            .unwrap();
        assert_eq!(store, common);

        // Carrying between two stores.
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        let source = object_path(&from, &oid);
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, content).unwrap();
        let absent = sha256(b"absent");
        let carried = carry(&from, &to, [oid.as_str(), absent.as_str()]).unwrap();
        assert_eq!(
            carried,
            Carried {
                copied: 1,
                missing: 1
            }
        );
        assert_eq!(std::fs::read(object_path(&to, &oid)).unwrap(), content);
        let again = carry(&from, &to, [oid.as_str()]).unwrap();
        assert_eq!(again, Carried::default(), "a second carry copies nothing");

        // An object whose content does not hash to its name is refused, and nothing of it is
        // left in the destination.
        let named = sha256(b"what the name promises");
        let forged = object_path(&from, &named);
        std::fs::create_dir_all(forged.parent().unwrap()).unwrap();
        std::fs::write(&forged, b"something else").unwrap();
        match carry(&from, &to, [named.as_str()]) {
            Err(Error::Mismatch { oid: o, .. }) => assert_eq!(o, named),
            other => panic!("a forged object is carried: {other:?}"),
        }
        let left: Vec<_> = std::fs::read_dir(object_path(&to, &named).parent().unwrap())
            .map_or_else(|_| Vec::new(), |d| d.map(|e| e.unwrap().path()).collect());
        assert_eq!(left, Vec::<PathBuf>::new());
        assert!(matches!(
            carry(&from, &to, ["../escape"]),
            Err(Error::Oid(_))
        ));
    }
}
