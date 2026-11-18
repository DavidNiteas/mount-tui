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
        // Unreadable (`EACCES`, `ENOTDIR`, ...): a path that cannot be
        // inspected is never reused — skip the candidate. Same orientation as
        // an unreadable directory below.
        Err(_) => return Ok(Slot::Taken),
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

/// Create the mount target of a sudo mount, if it has one.
///
/// Returns the path when this call created it (the caller removes it again
/// after a failed command); `None` when the directory already existed.
///
/// The path is checked twice: once before `create_dir_all` and once right after
/// it. The second check closes the window between the check and the creation, in
/// which the path could have been swapped for a symbolic link or something that
/// is not a directory.
pub fn prepare_mount_target(target: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let Some(target) = target else {
        return Ok(None);
    };
    let existed = target.exists();
    check_mount_target(target)?;
    std::fs::create_dir_all(target).map_err(|error| {
        format!(
            "failed to create mount point {}: {}",
            target.display(),
            error
        )
    })?;
    // TOCTOU re-check: never hand a swapped path to `sudo mount`. A path that
    // was replaced in this window is reported, not deleted: it is not ours.
    check_mount_target(target)?;
    Ok(if existed {
        None
    } else {
        Some(target.to_path_buf())
    })
}

/// Refuse a mount target that exists but is not a plain directory.
///
/// A symbolic link would redirect the mount somewhere unexpected; any other
/// non-directory means the mount cannot work, so it is reported before the
/// terminal is handed over. A missing or unreadable path is left to
/// `create_dir_all`, which reports the real error.
pub fn check_mount_target(target: &Path) -> Result<(), String> {
    let Ok(metadata) = std::fs::symlink_metadata(target) else {
        return Ok(());
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "mount point {} is a symbolic link",
            target.display()
        ));
    }
    if !file_type.is_dir() {
        return Err(format!(
            "mount point {} is not a directory",
            target.display()
        ));
    }
    Ok(())
}

/// Remove a mount point created by a failed run.
///
/// `remove_dir` refuses a non-empty directory, so content that appeared there
/// (a mount that partially succeeded) is never destroyed.
pub fn remove_empty_dir(path: &Path) {
    let _ = std::fs::remove_dir(path);
}

/// Removes the mount point created for one operation unless the command
/// succeeded.
///
/// The guard covers every early return after the target exists: a failing
/// command, a cancelled `sudo` or a panic must not leave a directory behind. A
/// successful mount calls [`CreatedDirGuard::disarm`], so the mount point is
/// kept.
pub struct CreatedDirGuard {
    created: Option<PathBuf>,
}

impl CreatedDirGuard {
    pub fn new(created: Option<PathBuf>) -> Self {
        Self { created }
    }

    /// Keep the directory (the mount succeeded).
    pub fn disarm(&mut self) {
        self.created = None;
    }
}

impl Drop for CreatedDirGuard {
    fn drop(&mut self) {
        if let Some(path) = self.created.take() {
            remove_empty_dir(&path);
        }
    }
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
    fn an_unreadable_candidate_is_taken() {
        let root = TempDir::new("unreadable");
        let file = root.path().join("plain-file");
        std::fs::write(&file, b"x").unwrap();

        // `<file>/child` cannot be inspected at all (ENOTDIR): a path that
        // cannot be checked is never reused.
        assert_eq!(slot(&file.join("child")).unwrap(), Slot::Taken);
        // A missing path is still reusable.
        assert_eq!(slot(&root.path().join("missing")).unwrap(), Slot::Reusable);
    }

    #[test]
    fn an_uninspectable_root_is_not_reused() {
        let root = TempDir::new("uninspectable-root");
        let file = root.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();

        // Every candidate lives below a plain file: all are skipped, so the
        // resolution reports `Exhausted` instead of pointing into a non-directory.
        let error = resolve_target(&file, &device("sdb1", Some("USBKEY"), None)).unwrap_err();
        assert!(
            matches!(error, TargetError::Exhausted { .. }),
            "expected Exhausted, got {:?}",
            error
        );
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
    fn a_missing_target_creates_nothing() {
        assert_eq!(prepare_mount_target(None), Ok(None));
    }

    #[test]
    fn preparing_creates_the_target_and_reports_it_created() {
        let root = TempDir::new("create");
        let target = root.path().join("usb").join("USBKEY");

        assert_eq!(
            prepare_mount_target(Some(&target)),
            Ok(Some(target.clone())),
            "a freshly created target is reported"
        );
        assert!(target.is_dir());
        // The parents (`usb`) are created as well.
        assert!(root.path().join("usb").is_dir());
    }

    #[test]
    fn preparing_reuses_an_existing_directory() {
        let root = TempDir::new("reuse");
        let target = root.path().join("USBKEY");
        std::fs::create_dir_all(&target).unwrap();

        assert_eq!(
            prepare_mount_target(Some(&target)),
            Ok(None),
            "an existing directory must not be reported as created"
        );
        assert!(target.is_dir());
    }

    #[test]
    fn preparing_reports_a_failure_instead_of_panicking() {
        let root = TempDir::new("blocked");
        let file = root.path().join("plain-file");
        std::fs::write(&file, b"x").unwrap();

        let error = prepare_mount_target(Some(&file.join("nested"))).unwrap_err();
        assert!(error.contains("failed to create mount point"), "{}", error);
    }

    #[test]
    fn preparing_refuses_a_symbolic_link() {
        let root = TempDir::new("symlink-target");
        let real = root.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.path().join("USBKEY");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let error = prepare_mount_target(Some(&link)).unwrap_err();
        assert!(error.contains("is a symbolic link"), "{}", error);
        assert!(link.is_symlink(), "the link must be left untouched");
    }

    #[test]
    fn preparing_refuses_a_non_directory() {
        let root = TempDir::new("file-target");
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();

        let error = prepare_mount_target(Some(&file)).unwrap_err();
        assert!(error.contains("is not a directory"), "{}", error);
        assert!(file.is_file(), "the existing file must survive");
    }

    #[test]
    fn the_target_checker_accepts_a_missing_path_and_a_real_directory() {
        let root = TempDir::new("checker");
        assert_eq!(check_mount_target(&root.path().join("missing")), Ok(()));

        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(check_mount_target(&nested), Ok(()));
    }

    #[test]
    fn the_cleanup_guard_removes_the_created_directory_on_drop() {
        let root = TempDir::new("guard-drop");
        let target = root.path().join("USBKEY");
        std::fs::create_dir_all(&target).unwrap();

        {
            let _guard = CreatedDirGuard::new(Some(target.clone()));
            // An early return (a cancelled command) leaves the scope without
            // disarming.
        }
        assert!(
            !target.exists(),
            "an operation that never mounted removes its mount point"
        );
    }

    #[test]
    fn the_cleanup_guard_keeps_the_directory_after_disarm() {
        let root = TempDir::new("guard-disarm");
        let target = root.path().join("USBKEY");
        std::fs::create_dir_all(&target).unwrap();

        {
            let mut guard = CreatedDirGuard::new(Some(target.clone()));
            guard.disarm();
        }
        assert!(target.is_dir(), "a successful mount keeps its mount point");
    }

    #[test]
    fn the_cleanup_guard_ignores_a_reused_directory_and_content() {
        let root = TempDir::new("guard-none");
        // Nothing was created by us: dropping the guard is a no-op.
        drop(CreatedDirGuard::new(None));

        // Content that appeared in the meantime (a partially successful mount)
        // must never be destroyed: only empty directories are removed.
        let occupied = root.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("data"), b"x").unwrap();
        drop(CreatedDirGuard::new(Some(occupied.clone())));
        assert!(occupied.join("data").is_file());
    }

    #[test]
    fn cleanup_removes_only_empty_directories() {
        let root = TempDir::new("cleanup");
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        remove_empty_dir(&empty);
        assert!(!empty.exists(), "an empty created mount point is removed");

        let occupied = root.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("data"), b"x").unwrap();
        remove_empty_dir(&occupied);
        assert!(occupied.is_dir(), "a non-empty directory must survive");

        // A missing path is a no-op, not a panic.
        remove_empty_dir(&root.path().join("missing"));
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
