//! Where a database directory lives: a local disk, a network file system or a synced folder.
//!
//! SQLite in WAL mode needs a local disk: its locking is unreliable over network file systems,
//! and a sync client copying the file and its `-wal` side file independently can corrupt it
//! (#3, #6). The check is a trait so tests can fake what the disk is.

use std::path::{Path, PathBuf};

use bosun_core::{PathStyle, canonical_path};

/// What kind of storage holds a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disk {
    /// A local disk, or nothing the probe can tell from one.
    Local,
    /// A network file system; the text says which and why.
    Network(String),
    /// A folder a sync client copies elsewhere; the text names the service.
    Synced(String),
}

/// Tells what kind of storage holds a directory.
pub trait DiskProbe {
    /// The storage of `dir`, which need not exist yet.
    fn probe(&self, dir: &Path) -> Disk;
}

/// The probe of the running system. Best effort: it refuses what it can see and says nothing
/// about the rest.
///
/// It sees:
///
/// - UNC paths (`\\server\share`), on Windows;
/// - mapped network drives and junctions or symbolic links into a share, on Windows: the
///   nearest existing ancestor of the directory is resolved by the OS, which names a network
///   location as `\\?\UNC\server\share`;
/// - network file systems on Linux (`/proc/self/mounts`) and macOS (the `mount` command):
///   NFS, SMB/CIFS, AFP, `WebDAV`, 9p (which includes WSL 2's view of Windows drives), AFS,
///   Ceph, `GlusterFS`, Lustre and the common FUSE network clients (sshfs, rclone, s3fs,
///   gcsfuse);
/// - `OneDrive`, at the folders named by `OneDrive`, `OneDriveConsumer` and
///   `OneDriveCommercial`, and at `~/OneDrive`;
/// - Dropbox, at the folders listed in its `info.json` (`%APPDATA%\Dropbox`,
///   `%LOCALAPPDATA%\Dropbox`, `~/.dropbox`), and at `~/Dropbox`;
/// - iCloud Drive at `~/iCloudDrive` (Windows) and `~/Library/Mobile Documents` (macOS);
/// - Google Drive at `~/Google Drive` and `~/My Drive`;
/// - everything under `~/Library/CloudStorage` (macOS File Provider: `OneDrive`, Dropbox,
///   Google Drive, Box and others).
///
/// It cannot see:
///
/// - a mapped drive that is disconnected, or any path none of whose ancestors exists;
/// - the Google Drive for desktop virtual drive on Windows (`G:` by default): it looks like a
///   local volume;
/// - folders synced by a client it does not know (Syncthing, Nextcloud, Box on Windows,
///   pCloud, Mega and so on), sync folders moved away from where their client says, and
///   macOS "Desktop and Documents Folders" in iCloud (`~/Documents` and `~/Desktop` look
///   local);
/// - network file systems under a FUSE or file-system type it does not list, and on other
///   platforms than Windows, Linux and macOS anything but the folders above.
#[derive(Debug, Clone)]
pub struct SystemProbe {
    /// Canonical keys of synced folders, lexical and resolved, with their service.
    synced: Vec<(String, &'static str)>,
    /// Mount points (canonical keys) with their file-system type.
    mounts: Vec<(String, String)>,
}

impl SystemProbe {
    /// Reads the environment, the sync clients' own records and the mount table once.
    #[must_use]
    pub fn new() -> Self {
        let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let read = |path: &Path| std::fs::read_to_string(path).ok();
        #[allow(
            clippy::disallowed_methods,
            reason = "`/sbin/mount` runs on macOS only, where no console can be gone"
        )]
        let mounts = if cfg!(target_os = "linux") {
            read(Path::new("/proc/self/mounts")).map(|text| linux_mounts(&text))
        } else if cfg!(target_os = "macos") {
            std::process::Command::new("/sbin/mount")
                .output()
                .ok()
                .map(|out| bsd_mounts(&String::from_utf8_lossy(&out.stdout)))
        } else {
            None
        };
        Self::from_parts(&synced_folders(&env, &read), mounts.unwrap_or_default())
    }

    fn from_parts(folders: &[(PathBuf, &'static str)], mounts: Vec<(String, String)>) -> Self {
        let mut synced = Vec::new();
        for (folder, service) in folders {
            for key in keys(folder) {
                synced.push((key, *service));
            }
        }
        Self { synced, mounts }
    }
}

impl Default for SystemProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskProbe for SystemProbe {
    fn probe(&self, dir: &Path) -> Disk {
        // The path as written first: a UNC path is refused before anything touches the network.
        keys(dir)
            .find_map(|key| self.judge(&key))
            .unwrap_or(Disk::Local)
    }
}

impl SystemProbe {
    fn judge(&self, key: &str) -> Option<Disk> {
        let style = PathStyle::host();
        if is_unc(key) {
            return Some(Disk::Network(format!("{key} is a network share")));
        }
        // The innermost mount holding the directory decides.
        if let Some((point, fs)) = self
            .mounts
            .iter()
            .filter(|(point, _)| within(key, point, style))
            .max_by_key(|(point, _)| point.len())
            && NETWORK_FILE_SYSTEMS.contains(&fs.as_str())
        {
            return Some(Disk::Network(format!(
                "{key} is on a {fs} mount at {point}"
            )));
        }
        self.synced
            .iter()
            .find(|(root, _)| within(key, root, style))
            .map(|(_, service)| Disk::Synced(format!("{key} is in a folder synced by {service}")))
    }
}

/// File-system types that live on another machine.
const NETWORK_FILE_SYSTEMS: &[&str] = &[
    "9p",
    "afpfs",
    "afs",
    "ceph",
    "cifs",
    "davfs",
    "fuse.gcsfuse",
    "fuse.glusterfs",
    "fuse.rclone",
    "fuse.s3fs",
    "fuse.sshfs",
    "glusterfs",
    "lustre",
    "ncpfs",
    "nfs",
    "nfs4",
    "smb3",
    "smbfs",
    "webdav",
];

fn is_unc(key: &str) -> bool {
    PathStyle::host() == PathStyle::Windows && key.starts_with(r"\\")
}

/// The canonical keys of `path`, lazily: as written, then as the OS resolves its nearest
/// existing ancestor (links, junctions, mapped drives) with the rest appended. A UNC path is
/// not resolved, so that the probe never waits on a network.
fn keys(path: &Path) -> impl Iterator<Item = String> {
    let lexical = canonical_path(&path.to_string_lossy(), PathStyle::host());
    let to_resolve = (!is_unc(&lexical)).then(|| path.to_owned());
    let resolved = to_resolve
        .into_iter()
        .filter_map(|path| resolved_key(&path));
    std::iter::once(lexical.clone()).chain(resolved.filter(move |key| *key != lexical))
}

fn resolved_key(path: &Path) -> Option<String> {
    path.ancestors().find_map(|ancestor| {
        let real = std::fs::canonicalize(ancestor).ok()?;
        let rest = path.strip_prefix(ancestor).ok()?;
        Some(canonical_path(
            &real.join(rest).to_string_lossy(),
            PathStyle::host(),
        ))
    })
}

/// Whether the canonical key `key` is `root` or inside it.
fn within(key: &str, root: &str, style: PathStyle) -> bool {
    let separator = match style {
        PathStyle::Windows => '\\',
        PathStyle::Unix => '/',
    };
    key.strip_prefix(root).is_some_and(|rest| {
        rest.is_empty() || root.ends_with(separator) || rest.starts_with(separator)
    })
}

/// The folders sync clients copy elsewhere, from the environment and the clients' records.
fn synced_folders(
    env: &dyn Fn(&str) -> Option<PathBuf>,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Vec<(PathBuf, &'static str)> {
    let env = |name: &str| env(name).filter(|path| path.is_absolute());
    let mut folders = Vec::new();
    for name in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Some(path) = env(name) {
            folders.push((path, "OneDrive"));
        }
    }

    let home = env(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let mut dropbox_records: Vec<PathBuf> = ["APPDATA", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(env)
        .map(|dir| dir.join("Dropbox").join("info.json"))
        .collect();
    if let Some(home) = &home {
        dropbox_records.push(home.join(".dropbox").join("info.json"));
    }
    for record in dropbox_records {
        if let Some(text) = read(&record) {
            for folder in dropbox_folders(&text) {
                folders.push((folder, "Dropbox"));
            }
        }
    }

    if let Some(home) = home {
        for (folder, service) in [
            (home.join("OneDrive"), "OneDrive"),
            (home.join("Dropbox"), "Dropbox"),
            (home.join("iCloudDrive"), "iCloud Drive"),
            (
                home.join("Library").join("Mobile Documents"),
                "iCloud Drive",
            ),
            (home.join("Google Drive"), "Google Drive"),
            (home.join("My Drive"), "Google Drive"),
            (
                home.join("Library").join("CloudStorage"),
                "a cloud storage provider",
            ),
        ] {
            folders.push((folder, service));
        }
    }
    folders
}

/// The folders of Dropbox's `info.json`: `{"personal": {"path": ...}, "business": {...}}`.
fn dropbox_folders(text: &str) -> Vec<PathBuf> {
    let Ok(serde_json::Value::Object(accounts)) = serde_json::from_str(text) else {
        return Vec::new();
    };
    accounts
        .values()
        .filter_map(|account| account.get("path")?.as_str())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect()
}

/// Mount points and types from `/proc/self/mounts`: `device point type options 0 0`, with
/// spaces and other bytes in the point escaped as `\ooo`.
fn linux_mounts(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _device = fields.next()?;
            let point = unescape_octal(fields.next()?);
            let fs = fields.next()?;
            Some((canonical_path(&point, PathStyle::Unix), fs.to_owned()))
        })
        .collect()
}

fn unescape_octal(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let digits = bytes.get(i + 1..i + 4);
        if bytes[i] == b'\\'
            && let Some(digits) = digits
            && digits.iter().all(|b| (b'0'..=b'7').contains(b))
        {
            let value = digits
                .iter()
                .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount points and types from the BSD `mount` command:
/// `//user@host/share on /Volumes/share (smbfs, nodev, nosuid, mounted by user)`.
fn bsd_mounts(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (_device, rest) = line.split_once(" on ")?;
            let open = rest.rfind(" (")?;
            let point = &rest[..open];
            let fs = rest[open + 2..].split([',', ')']).next()?.trim();
            Some((canonical_path(point, PathStyle::Unix), fs.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_matches_whole_components() {
        let w = PathStyle::Windows;
        assert!(within(
            r"c:\users\me\onedrive\db",
            r"c:\users\me\onedrive",
            w
        ));
        assert!(within(r"c:\users\me\onedrive", r"c:\users\me\onedrive", w));
        assert!(within(r"c:\x", r"c:\", w));
        assert!(!within(
            r"c:\users\me\onedrive-old\db",
            r"c:\users\me\onedrive",
            w
        ));
        let u = PathStyle::Unix;
        assert!(within("/mnt/nfs/db", "/mnt/nfs", u));
        assert!(within("/anything", "/", u));
        assert!(!within("/mnt/nfs2/db", "/mnt/nfs", u));
    }

    #[test]
    fn linux_mount_table() {
        let text = "/dev/sda1 / ext4 rw,relatime 0 0\n\
                    server:/export /mnt/my\\040share nfs4 rw 0 0\n\
                    //host/s /mnt/smb cifs rw 0 0\n\
                    garbage\n";
        assert_eq!(
            linux_mounts(text),
            [
                ("/".to_owned(), "ext4".to_owned()),
                ("/mnt/my share".to_owned(), "nfs4".to_owned()),
                ("/mnt/smb".to_owned(), "cifs".to_owned()),
            ]
        );
        assert_eq!(unescape_octal(r"a\134b\0"), r"a\b\0");
    }

    #[test]
    fn bsd_mount_output() {
        let text = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                    //me@nas/Data on /Volumes/Data (smbfs, nodev, nosuid, mounted by me)\n\
                    map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n";
        assert_eq!(
            bsd_mounts(text),
            [
                ("/".to_owned(), "apfs".to_owned()),
                ("/Volumes/Data".to_owned(), "smbfs".to_owned()),
                ("/System/Volumes/Data/home".to_owned(), "autofs".to_owned()),
            ]
        );
    }

    /// An absolute path on this host under a synthetic root that does not exist.
    fn abs(rest: &str) -> PathBuf {
        let root = if cfg!(windows) {
            r"Z:\bosun-test-root"
        } else {
            "/bosun-test-root"
        };
        Path::new(root).join(rest)
    }

    #[test]
    fn synced_folders_from_the_environment_and_dropbox_records() {
        let home = abs("home");
        let appdata = abs("appdata");
        let dropbox_record = appdata.join("Dropbox").join("info.json");
        let team = abs("Team Dropbox");
        let env = |name: &str| match name {
            "OneDriveCommercial" => Some(abs("OneDrive - Org")),
            "OneDrive" => Some(PathBuf::from("relative")),
            "USERPROFILE" | "HOME" => Some(home.clone()),
            "APPDATA" => Some(appdata.clone()),
            _ => None,
        };
        let record = serde_json::json!({ "business": { "path": team.to_string_lossy() } });
        let read = |path: &Path| (path == dropbox_record).then(|| record.to_string());

        let folders = synced_folders(&env, &read);
        assert!(folders.contains(&(abs("OneDrive - Org"), "OneDrive")));
        assert!(folders.contains(&(team, "Dropbox")));
        assert!(folders.contains(&(home.join("Dropbox"), "Dropbox")));
        assert!(
            !folders
                .iter()
                .any(|(path, _)| path == Path::new("relative")),
            "a relative variable is ignored"
        );
    }

    #[test]
    fn the_system_probe_refuses_synced_and_network_folders() {
        let probe = SystemProbe::from_parts(
            &[(abs("OneDrive"), "OneDrive")],
            vec![
                ("/mnt/nfs".to_owned(), "nfs".to_owned()),
                ("/mnt/nfs/disk".to_owned(), "ext4".to_owned()),
            ],
        );
        assert!(matches!(
            probe.probe(&abs("OneDrive").join("bosun")),
            Disk::Synced(_)
        ));
        assert_eq!(probe.probe(&abs("OneDrive-old")), Disk::Local);
        if cfg!(windows) {
            assert!(matches!(
                probe.probe(Path::new(r"\\server\share\bosun")),
                Disk::Network(_)
            ));
            assert!(matches!(
                probe.probe(Path::new(r"\\?\UNC\server\share\bosun")),
                Disk::Network(_)
            ));
        } else {
            assert!(matches!(
                probe.probe(Path::new("/mnt/nfs/bosun")),
                Disk::Network(_)
            ));
            assert_eq!(probe.probe(Path::new("/mnt/nfs/disk/bosun")), Disk::Local);
        }
    }

    #[test]
    fn the_temporary_directory_is_local() {
        let dir = std::env::temp_dir().join("bosun-config-probe");
        assert_eq!(SystemProbe::new().probe(&dir), Disk::Local);
    }
}
