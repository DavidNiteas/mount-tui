//! Mount point policy for sudo mode: target naming under the configured root and
//! the option string handed to `sudo mount`.
//!
//! Everything here is a pure function of the device plus the filesystem state
//! (for conflict detection, nothing is ever created), so the whole policy is
//! unit-testable without a real mount.

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::disk::BlockDevice;

/// Options every sudo mount gets: a removable filesystem must never expose
/// setuid binaries or device nodes on the host.
pub const BASE_MOUNT_OPTIONS: &str = "nosuid,nodev";

/// Filesystems with a single-owner mapping: without `uid`/`gid` the mounted
/// files would belong to root and be unusable for the invoking user.
const OWNER_MOUNT_FSTYPES: &[&str] = &["vfat", "exfat", "ntfs", "ntfs3", "iso9660"];

/// umask applied to files created on those filesystems.
const OWNER_UMASK: &str = "0022";

/// Highest numeric conflict suffix tried before giving up.
const MAX_SUFFIX: u32 = 99;

/// Why a target path could not be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// The candidate is a symbolic link: following it would mount somewhere
    /// unexpected, so it is refused instead of replaced.
    Symlink(PathBuf),
    /// Every candidate name under the root is already taken.
    Exhausted { root: PathBuf, base: String },
}

impl fmt::Display for TargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink(path) => {
                write!(
                    formatter,
                    "mount point {} is a symbolic link",
                    path.display()
                )
            }
            Self::Exhausted { root, base } => write!(
                formatter,
                "no free mount point name for '{}' under {}",
                base,
                root.display()
            ),
        }
    }
}

impl std::error::Error for TargetError {}

/// Directory name for `device`: the sanitised label when it has one, the device
/// name otherwise.
///
/// `/`, `\` and control characters become `_`, because a label is arbitrary
/// udev data and must never introduce a path separator. An empty result, `.` or
/// `..` fall back to the device name.
pub fn target_name(device: &BlockDevice) -> String {
    let label = device.label.as_deref().unwrap_or_default().trim();
    if label.is_empty() {
        return device.name.clone();
    }
    let sanitized: String = label
        .chars()
        .map(|character| match character {
            '/' | '\\' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect();
    let sanitized = sanitized.trim();
    match sanitized {
        "" | "." | ".." => device.name.clone(),
        name => name.to_string(),
    }
}

/// Pick the mount target for `device` under `root`.
///
/// * a free name, an existing empty directory, or an empty directory reached
///   through the conflict suffixes is reused;
/// * an existing non-empty directory, a mount point of another device or a
///   plain file forces the next candidate: `<base>-<name>`, then `<base>-2`,
///   `<base>-3`, …;
/// * a symbolic link is refused.
///
/// The chosen path is returned, never created: `main` creates it right before
/// the terminal is handed to `sudo`.
pub fn resolve_target(root: &Path, device: &BlockDevice) -> Result<PathBuf, TargetError> {
    let base = target_name(device);
    let first = root.join(&base);
    if slot(&first)? == Slot::Reusable {
        return Ok(first);
    }
    for name in conflict_names(&base, &device.name) {
        let candidate = root.join(name);
        if slot(&candidate)? == Slot::Reusable {
            return Ok(candidate);
        }
    }
    Err(TargetError::Exhausted {
        root: root.to_path_buf(),
        base,
    })
}

/// Candidate names after `<base>` itself was taken.
fn conflict_names(base: &str, device_name: &str) -> Vec<String> {
    let mut names = vec![format!("{}-{}", base, device_name)];
    names.extend((2..=MAX_SUFFIX).map(|index| format!("{}-{}", base, index)));
    names
}

/// Whether an existing path may be used as a mount point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Missing, or an existing empty directory that is not a mount point.
    Reusable,
    /// Occupied by something a mount would hide or clobber.
    Taken,
}

fn slot(path: &Path) -> Result<Slot, TargetError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        // Nothing there yet: `create_dir_all` will make it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Slot::Reusable),
        // Unreadable: let the actual `mkdir` / `mount` report the real error.
        Err(_) => return Ok(Slot::Reusable),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(TargetError::Symlink(path.to_path_buf()));
    }
    if !file_type.is_dir() {
        return Ok(Slot::Taken);
    }
    if is_mount_point(path) {
        // Another filesystem is mounted here: mounting on top would stack them.
        return Ok(Slot::Taken);
    }
    match std::fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                Ok(Slot::Taken)
            } else {
                Ok(Slot::Reusable)
            }
        }
        Err(_) => Ok(Slot::Taken),
    }
}

/// True when `path` is a mount point of its own.
///
/// A mount point has a different `st_dev` than its parent directory, which is
/// cheaper and more robust than parsing `/proc/self/mountinfo`.
fn is_mount_point(path: &Path) -> bool {
    let (Some(parent), Ok(metadata)) = (path.parent(), std::fs::metadata(path)) else {
        return false;
    };
    let Ok(parent_metadata) = std::fs::metadata(parent) else {
        return false;
    };
    metadata.dev() != parent_metadata.dev()
}

/// `sudo mount -o` options for `device`.
///
/// The base options always apply; `owner` (uid/gid of the invoking user) is
/// added only for filesystems that map every file to a single user.
pub fn mount_options(device: &BlockDevice, owner: Option<(u32, u32)>) -> String {
    let mut options = BASE_MOUNT_OPTIONS.to_string();
    let fstype = device
        .fstype
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if let Some((uid, gid)) = owner.filter(|_| OWNER_MOUNT_FSTYPES.contains(&fstype.as_str())) {
        options.push_str(&format!(",uid={},gid={},umask={}", uid, gid, OWNER_UMASK));
    }
    options
}

/// `(uid, gid)` of `$HOME`: the user the mounted files should belong to.
///
/// Read from the home directory's metadata instead of adding a `libc`
/// dependency; `None` simply leaves the owner options off.
pub fn home_owner() -> Option<(u32, u32)> {
    let home = std::env::var_os("HOME")?;
    let metadata = std::fs::metadata(home).ok()?;
    Some((metadata.uid(), metadata.gid()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique scratch directory; removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "mount-tui-mount-{}-{}-{}",
                tag,
                std::process::id(),
                unique
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn device(name: &str, label: Option<&str>, fstype: Option<&str>) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            size: "1G".to_string(),
            device_type: "part".to_string(),
            fstype: fstype.map(str::to_string),
            mountpoints: Vec::new(),
            label: label.map(str::to_string),
            ro: false,
            rm: false,
            model: None,
            vendor: None,
            tran: None,
            serial: None,
            uuid: None,
            children: None,
        }
    }

    #[test]
    fn label_is_trimmed_and_sanitised() {
        let cases = [
            (Some("USBKEY"), "USBKEY"),
            (Some("  USBKEY  "), "USBKEY"),
            (Some("My Disk"), "My Disk"),
            (Some("a/b"), "a_b"),
            (Some("a\\b"), "a_b"),
            (Some("tab\tnl"), "tab_nl"),
            (Some("bell\u{7}here"), "bell_here"),
            (Some("../../etc"), ".._.._etc"),
            (Some(""), "sdb1"),
            (Some("   "), "sdb1"),
            (Some("."), "sdb1"),
            (Some(".."), "sdb1"),
            (None, "sdb1"),
        ];
        for (label, expected) in cases {
            assert_eq!(
                target_name(&device("sdb1", label, Some("vfat"))),
                expected,
                "label {:?}",
                label
            );
        }
    }

    #[test]
    fn a_missing_target_is_used_as_is() {
        let root = TempDir::new("missing");
        let target = resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap();
        assert_eq!(target, root.path().join("USBKEY"));
        assert!(!target.exists(), "resolving must not create anything");
    }

    #[test]
    fn an_empty_directory_is_reused() {
        let root = TempDir::new("empty");
        let target = root.path().join("USBKEY");
        std::fs::create_dir_all(&target).unwrap();
        assert_eq!(
            resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap(),
            target
        );
    }

    #[test]
    fn a_non_empty_directory_falls_back_to_the_suffixed_names() {
        let root = TempDir::new("conflict");
        let base = root.path().join("USBKEY");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("keep.txt"), b"x").unwrap();

        let target = resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap();
        assert_eq!(target, root.path().join("USBKEY-sdb1"));

        // Second-level conflict: <base>-<name> is taken too, so <base>-2 wins.
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"x").unwrap();
        let target = resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap();
        assert_eq!(target, root.path().join("USBKEY-2"));

        // Third-level conflict: <base>-2 is taken as well → <base>-3.
        std::fs::create_dir_all(root.path().join("USBKEY-2")).unwrap();
        std::fs::write(root.path().join("USBKEY-2/keep.txt"), b"x").unwrap();
        let target = resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap();
        assert_eq!(target, root.path().join("USBKEY-3"));
    }

    #[test]
    fn a_plain_file_is_skipped() {
        let root = TempDir::new("file");
        std::fs::write(root.path().join("USBKEY"), b"not a directory").unwrap();
        let target = resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap();
        assert_eq!(target, root.path().join("USBKEY-sdb1"));
    }

    #[test]
    fn a_symlink_target_is_refused() {
        let root = TempDir::new("symlink");
        let real = root.path().join("real-target");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.path().join("USBKEY");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let error = resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap_err();
        assert_eq!(error, TargetError::Symlink(link.clone()));
        assert!(error.to_string().contains("symbolic link"), "{}", error);
    }

    #[test]
    fn a_device_without_label_uses_its_name() {
        let root = TempDir::new("noname");
        let target = resolve_target(root.path(), &device("sdz1", None, None)).unwrap();
        assert_eq!(target, root.path().join("sdz1"));
    }

    #[test]
    fn exhausted_names_report_the_root_and_base() {
        let root = TempDir::new("exhausted");
        for name in std::iter::once("USBKEY")
            .chain(conflict_names("USBKEY", "sdb1").iter().map(String::as_str))
        {
            let path = root.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("keep"), b"x").unwrap();
        }
        let error = resolve_target(root.path(), &device("sdb1", Some("USBKEY"), None)).unwrap_err();
        assert_eq!(
            error,
            TargetError::Exhausted {
                root: root.path().to_path_buf(),
                base: "USBKEY".to_string(),
            }
        );
        assert!(
            error.to_string().contains("no free mount point name"),
            "{}",
            error
        );
    }

    #[test]
    fn base_options_always_apply() {
        assert_eq!(
            mount_options(&device("sdb1", None, Some("ext4")), Some((1000, 1000))),
            "nosuid,nodev"
        );
        assert_eq!(
            mount_options(&device("sdb1", None, None), None),
            BASE_MOUNT_OPTIONS
        );
    }

    #[test]
    fn owner_options_follow_the_filesystem() {
        for fstype in ["vfat", "exfat", "ntfs", "ntfs3", "iso9660", "VFAT"] {
            assert_eq!(
                mount_options(&device("sdb1", None, Some(fstype)), Some((1000, 1001))),
                "nosuid,nodev,uid=1000,gid=1001,umask=0022",
                "fstype {}",
                fstype
            );
        }
        for fstype in ["ext4", "btrfs", "xfs", "hfsplus"] {
            assert_eq!(
                mount_options(&device("sdb1", None, Some(fstype)), Some((1000, 1001))),
                "nosuid,nodev",
                "fstype {}",
                fstype
            );
        }
        // Without an owner the filesystem keeps the base options only.
        assert_eq!(
            mount_options(&device("sdb1", None, Some("vfat")), None),
            "nosuid,nodev"
        );
    }

    #[test]
    fn home_owner_reads_the_home_metadata() {
        // `HOME` is process-global; this only reads it, and the expectation is
        // derived from the same directory, so it holds for any value.
        if let Some(owner) = home_owner() {
            let metadata = std::fs::metadata(std::env::var_os("HOME").unwrap()).unwrap();
            assert_eq!(owner, (metadata.uid(), metadata.gid()));
        }
    }

    #[test]
    fn a_real_directory_is_not_a_mount_point() {
        let root = TempDir::new("not-a-mount");
        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(!is_mount_point(&nested));
        // A path that does not exist cannot be a mount point.
        assert!(!is_mount_point(&root.path().join("missing")));
    }
}
