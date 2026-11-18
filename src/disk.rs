use serde::{Deserialize, Deserializer};

use crate::runner::{CommandOutput, CommandRunner, CommandSpec};

/// Preferred `lsblk` columns (`MOUNTPOINTS` is absent from older lsblk builds).
const LSBLK_COLUMNS: &str =
    "NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT,MOUNTPOINTS,LABEL,MODEL,VENDOR,TRAN,SERIAL,UUID,RO,RM";

/// Legacy column set retried when the preferred one is rejected: only
/// `MOUNTPOINTS` is dropped, `RO`/`RM` predate it and stay.
const LSBLK_COLUMNS_LEGACY: &str =
    "NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT,LABEL,MODEL,VENDOR,TRAN,SERIAL,UUID,RO,RM";

#[derive(Debug, Clone, PartialEq)]
pub struct BlockDevice {
    pub name: String,
    pub size: String,
    pub device_type: String,
    pub fstype: Option<String>,
    /// Every mount point of the device: empty strings and duplicates removed,
    /// original order preserved.
    pub mountpoints: Vec<String>,
    pub label: Option<String>,
    pub ro: bool,
    pub rm: bool,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub tran: Option<String>,
    pub serial: Option<String>,
    pub uuid: Option<String>,
    pub children: Option<Vec<BlockDevice>>,
}

/// Raw `lsblk --json` record.
///
/// Older lsblk emits only `mountpoint`; current lsblk emits both `mountpoint`
/// and `mountpoints` (where unmounted devices show `[null]`). Both shapes are
/// accepted and normalised by [`BlockDevice::from`].
#[derive(Debug, Clone, Deserialize)]
struct RawBlockDevice {
    name: String,
    size: String,
    #[serde(rename = "type")]
    device_type: String,
    fstype: Option<String>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default, deserialize_with = "deserialize_mountpoints")]
    mountpoints: Vec<Option<String>>,
    label: Option<String>,
    #[serde(default)]
    ro: bool,
    #[serde(default)]
    rm: bool,
    model: Option<String>,
    vendor: Option<String>,
    tran: Option<String>,
    serial: Option<String>,
    uuid: Option<String>,
    children: Option<Vec<RawBlockDevice>>,
}

/// `mountpoints` may be missing, `null` or `[null]`; all collapse to an empty
/// list instead of failing the whole parse.
fn deserialize_mountpoints<'de, D>(deserializer: D) -> Result<Vec<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<Option<String>>>::deserialize(deserializer)?.unwrap_or_default())
}

impl From<RawBlockDevice> for BlockDevice {
    fn from(raw: RawBlockDevice) -> Self {
        let mut mountpoints = dedupe_non_empty(raw.mountpoints.into_iter().flatten());
        if mountpoints.is_empty() {
            if let Some(mountpoint) = raw.mountpoint.filter(|mp| !mp.is_empty()) {
                mountpoints.push(mountpoint);
            }
        }
        Self {
            name: raw.name,
            size: raw.size,
            device_type: raw.device_type,
            fstype: raw.fstype,
            mountpoints,
            label: raw.label,
            ro: raw.ro,
            rm: raw.rm,
            model: raw.model,
            vendor: raw.vendor,
            tran: raw.tran,
            serial: raw.serial,
            uuid: raw.uuid,
            children: raw
                .children
                .map(|children| children.into_iter().map(BlockDevice::from).collect()),
        }
    }
}

/// Drop empty strings and duplicates, keeping the first occurrence order.
fn dedupe_non_empty(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for value in values {
        if value.is_empty() || result.contains(&value) {
            continue;
        }
        result.push(value);
    }
    result
}

#[derive(Debug, Clone, Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<RawBlockDevice>,
}

/// Mount points that are always protected from unmounting.
const PROTECTED_MOUNTPOINTS: &[&str] = &[
    "/", "/boot", "/home", "/usr", "/var", "/etc", "/opt", "/root", "/srv", "/tmp",
];

/// Prefixes whose sub-mounts are protected as well. `/home/` is intentionally
/// absent: this tool mounts under `$HOME/mnt/<name>`, which must stay unmountable.
const PROTECTED_PREFIXES: &[&str] = &["/boot/", "/usr/", "/var/", "/etc/", "/opt/", "/srv/"];

pub fn get_block_devices(runner: &dyn CommandRunner) -> anyhow::Result<Vec<BlockDevice>> {
    let preferred = run_lsblk(runner, LSBLK_COLUMNS)?;
    let output = if preferred.success {
        preferred
    } else {
        // Older lsblk rejects unknown columns: retry without `MOUNTPOINTS`.
        let legacy = run_lsblk(runner, LSBLK_COLUMNS_LEGACY)?;
        if !legacy.success {
            anyhow::bail!(
                "lsblk failed with MOUNTPOINTS ({}) and without it ({})",
                describe_stderr(&preferred),
                describe_stderr(&legacy)
            );
        }
        legacy
    };
    parse_devices(&output.stdout)
}

fn run_lsblk(runner: &dyn CommandRunner, columns: &str) -> anyhow::Result<CommandOutput> {
    Ok(runner.output(&CommandSpec::new("lsblk", &["-o", columns, "--json"]))?)
}

fn describe_stderr(output: &CommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        match output.code {
            Some(code) => format!("exit code {}", code),
            None => "a signal".to_string(),
        }
    } else {
        stderr
    }
}

fn parse_devices(stdout: &[u8]) -> anyhow::Result<Vec<BlockDevice>> {
    let parsed: LsblkOutput = serde_json::from_slice(stdout)?;
    let mut devices: Vec<BlockDevice> = parsed
        .blockdevices
        .into_iter()
        .map(BlockDevice::from)
        .collect();
    // Partitions report `tran: null`; inherit the parent's transport so USB
    // partitions are recognised.
    inherit_transport(&mut devices, None);
    Ok(devices)
}

/// Recursively propagate the parent transport to children that lack one.
fn inherit_transport(devices: &mut [BlockDevice], parent_tran: Option<&str>) {
    for device in devices {
        let tran = device
            .tran
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| parent_tran.map(str::to_string));
        device.tran = tran.clone();
        if let Some(children) = device.children.as_mut() {
            inherit_transport(children, tran.as_deref());
        }
    }
}

pub fn is_usb(device: &BlockDevice) -> bool {
    device
        .tran
        .as_deref()
        .is_some_and(|tran| tran.eq_ignore_ascii_case("usb"))
}

pub fn is_mountable(device: &BlockDevice) -> bool {
    if device.device_type == "loop" {
        return false;
    }
    match device.fstype.as_deref() {
        None | Some("") | Some("swap") => false,
        Some(fstype) => !fstype.ends_with("_member") && !fstype.starts_with("crypto_"),
    }
}

pub fn is_mounted(device: &BlockDevice) -> bool {
    !device.mountpoints.is_empty()
}

/// True when **any** mount point of the device must never be unmounted
/// (safety first: a second, benign mount does not weaken the protection).
pub fn is_system_mount(device: &BlockDevice) -> bool {
    device.mountpoints.iter().any(|mp| mountpoint_is_system(mp))
}

fn mountpoint_is_system(mountpoint: &str) -> bool {
    // Normalise a trailing slash (`/home/` == `/home`); keep `/` untouched.
    let normalized = if mountpoint.len() > 1 {
        mountpoint.trim_end_matches('/')
    } else {
        mountpoint
    };
    if normalized.is_empty() {
        return false;
    }
    PROTECTED_MOUNTPOINTS.contains(&normalized)
        || PROTECTED_PREFIXES
            .iter()
            .any(|prefix| normalized.starts_with(prefix))
        || is_home_directory(normalized)
}

/// `/home/<single user>` (exactly two path segments) is protected so a home
/// directory mounted on its own cannot be lazy-unmounted. Deeper paths such as
/// `/home/<user>/mnt/<name>` stay unmountable.
fn is_home_directory(normalized: &str) -> bool {
    let mut segments = normalized.split('/');
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next()
        ),
        (Some(""), Some("home"), Some(user), None) if !user.is_empty()
    )
}

pub fn device_path(name: &str) -> String {
    format!("/dev/{}", name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::FakeRunner;

    fn bd(
        name: &str,
        device_type: &str,
        fstype: Option<&str>,
        mountpoint: Option<&str>,
    ) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            size: "1G".to_string(),
            device_type: device_type.to_string(),
            fstype: fstype.map(str::to_string),
            mountpoints: mountpoint.map(str::to_string).into_iter().collect(),
            label: None,
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

    fn with_mounts(mut device: BlockDevice, mountpoints: &[&str]) -> BlockDevice {
        device.mountpoints = mountpoints.iter().map(|mp| mp.to_string()).collect();
        device
    }

    #[test]
    fn protects_root_and_top_level_system_mounts() {
        for mp in [
            "/", "/boot", "/home", "/usr", "/var", "/etc", "/opt", "/root", "/srv", "/tmp",
        ] {
            let device = bd("sda1", "part", Some("ext4"), Some(mp));
            assert!(is_system_mount(&device), "{} should be protected", mp);
        }
    }

    #[test]
    fn protects_listed_system_prefixes() {
        for mp in [
            "/boot/efi",
            "/usr/local",
            "/var/lib",
            "/etc/ssh",
            "/opt/tools",
            "/srv/data",
        ] {
            let device = bd("sda1", "part", Some("ext4"), Some(mp));
            assert!(is_system_mount(&device), "{} should be protected", mp);
        }
    }

    #[test]
    fn protects_single_level_home_directories() {
        for mp in ["/home/user", "/home/alice", "/home/user/", "/home/"] {
            let device = bd("sda1", "part", Some("ext4"), Some(mp));
            assert!(is_system_mount(&device), "{} should be protected", mp);
        }
    }

    #[test]
    fn normalises_trailing_slash_before_matching() {
        for mp in [
            "/", "/boot/", "/usr/", "/var/", "/etc/", "/opt/", "/root/", "/srv/", "/tmp/",
        ] {
            let device = bd("sda1", "part", Some("ext4"), Some(mp));
            assert!(is_system_mount(&device), "{} should be protected", mp);
        }
    }

    #[test]
    fn does_not_protect_user_and_removable_mounts() {
        for mp in [
            "/home/user/mnt/sdb1",
            "/home/user/data",
            "/home/user/mnt/sdb1/",
            "/mnt/backup",
            "/media/user/USB",
            "/rhome",
            "/bootable",
            "/usrlocal",
            "/tmpfoo",
        ] {
            let device = bd("sdb1", "part", Some("vfat"), Some(mp));
            assert!(!is_system_mount(&device), "{} must stay unmountable", mp);
        }
    }

    #[test]
    fn unmounted_device_is_not_system_mount() {
        let device = bd("sdb1", "part", Some("vfat"), None);
        assert!(!is_system_mount(&device));
        assert!(!is_mounted(&device));
    }

    #[test]
    fn is_mountable_accepts_real_filesystems() {
        for fs in ["ext4", "vfat", "ntfs", "btrfs", "xfs", "exfat"] {
            let device = bd("sdb1", "part", Some(fs), None);
            assert!(is_mountable(&device), "{} should be mountable", fs);
        }
    }

    #[test]
    fn is_mountable_rejects_non_filesystems() {
        let cases = [
            bd("sdb1", "part", None, None),
            bd("sdb1", "part", Some(""), None),
            bd("sda2", "part", Some("swap"), None),
            bd("sda3", "lvm", Some("LVM2_member"), None),
            bd("sda4", "part", Some("crypto_LUKS"), None),
            bd("loop0", "loop", Some("ext4"), None),
        ];
        for device in cases {
            assert!(
                !is_mountable(&device),
                "{} ({:?}) must not be mountable",
                device.name,
                device.fstype
            );
        }
    }

    #[test]
    fn transport_is_inherited_recursively() {
        let mut devices = vec![BlockDevice {
            children: Some(vec![BlockDevice {
                fstype: Some("vfat".to_string()),
                children: Some(vec![bd("sdb1-crypt", "crypt", Some("ext4"), None)]),
                ..bd("sdb1", "part", Some("vfat"), None)
            }]),
            tran: Some("usb".to_string()),
            ..bd("sdb", "disk", None, None)
        }];

        inherit_transport(&mut devices, None);

        let child = &devices[0].children.as_ref().unwrap()[0];
        assert_eq!(child.tran.as_deref(), Some("usb"));
        assert!(is_usb(child));
        let grandchild = &child.children.as_ref().unwrap()[0];
        assert_eq!(grandchild.tran.as_deref(), Some("usb"));
    }

    #[test]
    fn own_transport_wins_over_parent() {
        let mut devices = vec![BlockDevice {
            children: Some(vec![BlockDevice {
                tran: Some("nvme".to_string()),
                ..bd("nvme0n1p1", "part", Some("ext4"), None)
            }]),
            tran: Some("usb".to_string()),
            ..bd("nvme0n1", "disk", None, None)
        }];

        inherit_transport(&mut devices, None);

        let child = &devices[0].children.as_ref().unwrap()[0];
        assert_eq!(child.tran.as_deref(), Some("nvme"));
        assert!(!is_usb(child));
    }

    #[test]
    fn lsblk_json_parses_label_and_children() {
        let json = r#"{"blockdevices":[{"name":"sdb","size":"8G","type":"disk","fstype":null,"mountpoint":null,"label":null,"model":"USB","vendor":"Generic","tran":"usb","serial":"123","uuid":null,"children":[{"name":"sdb1","size":"8G","type":"part","fstype":"vfat","mountpoint":"/media/u/USB","label":"USBKEY","model":null,"vendor":null,"tran":null,"serial":null,"uuid":"ABCD"}]}]}"#;
        let devices = parse_devices(json.as_bytes()).unwrap();
        let disk = &devices[0];
        let part = &disk.children.as_ref().unwrap()[0];
        assert_eq!(part.label.as_deref(), Some("USBKEY"));
        assert_eq!(part.mountpoints, vec!["/media/u/USB".to_string()]);
        assert!(is_usb(part));
        assert!(is_mountable(part));
    }

    #[test]
    fn new_format_parses_multiple_mountpoints_and_flags() {
        let json = r#"{"blockdevices":[{"name":"sdb1","size":"8G","type":"part","fstype":"ext4","mountpoint":"/mnt/a","mountpoints":["/mnt/a","/mnt/b"],"label":"DATA","ro":true,"rm":true,"model":null,"vendor":null,"tran":"usb","serial":null,"uuid":null,"children":null}]}"#;
        let devices = parse_devices(json.as_bytes()).unwrap();
        let device = &devices[0];
        assert_eq!(
            device.mountpoints,
            vec!["/mnt/a".to_string(), "/mnt/b".to_string()]
        );
        assert!(device.ro);
        assert!(device.rm);
        assert_eq!(device.label.as_deref(), Some("DATA"));
        assert!(is_mounted(device));
    }

    #[test]
    fn legacy_format_falls_back_to_single_mountpoint() {
        let json = r#"{"blockdevices":[{"name":"sdc1","size":"4G","type":"part","fstype":"vfat","mountpoint":"/media/u/OLD","label":"OLD","model":null,"vendor":null,"tran":null,"serial":null,"uuid":null}]}"#;
        let devices = parse_devices(json.as_bytes()).unwrap();
        let device = &devices[0];
        assert_eq!(device.mountpoints, vec!["/media/u/OLD".to_string()]);
        // Missing RO / RM columns default to false.
        assert!(!device.ro);
        assert!(!device.rm);
    }

    #[test]
    fn legacy_format_keeps_ro_rm_flags() {
        // Legacy lsblk (no MOUNTPOINTS) still reports RO / RM.
        let json = r#"{"blockdevices":[{"name":"sdc1","size":"4G","type":"part","fstype":"vfat","mountpoint":"/media/u/RO","label":"ROFS","ro":true,"rm":true,"model":null,"vendor":null,"tran":null,"serial":null,"uuid":null}]}"#;
        let device = &parse_devices(json.as_bytes()).unwrap()[0];
        assert_eq!(device.mountpoints, vec!["/media/u/RO".to_string()]);
        assert!(device.ro);
        assert!(device.rm);
    }

    #[test]
    fn legacy_columns_drop_only_mountpoints() {
        assert!(LSBLK_COLUMNS.contains("MOUNTPOINTS"));
        assert!(!LSBLK_COLUMNS_LEGACY.contains("MOUNTPOINTS"));
        // RO / RM predate MOUNTPOINTS and must survive the fallback.
        assert!(LSBLK_COLUMNS_LEGACY.ends_with(",RO,RM"));
    }

    #[test]
    fn null_or_empty_mountpoints_do_not_fail_parsing() {
        // `[null]` (unmounted device from a current lsblk).
        let json = r#"{"blockdevices":[{"name":"sdb1","size":"8G","type":"part","fstype":"vfat","mountpoint":null,"mountpoints":[null],"label":null,"ro":false,"rm":false,"model":null,"vendor":null,"tran":null,"serial":null,"uuid":null}]}"#;
        let device = &parse_devices(json.as_bytes()).unwrap()[0];
        assert!(device.mountpoints.is_empty());
        assert!(!is_mounted(device));

        // Whole array is null.
        let json = r#"{"blockdevices":[{"name":"sdb1","size":"8G","type":"part","fstype":"vfat","mountpoint":null,"mountpoints":null}]}"#;
        assert!(parse_devices(json.as_bytes()).unwrap()[0]
            .mountpoints
            .is_empty());

        // Key missing entirely.
        let json =
            r#"{"blockdevices":[{"name":"sdb1","size":"8G","type":"part","fstype":"vfat"}]}"#;
        let device = &parse_devices(json.as_bytes()).unwrap()[0];
        assert!(device.mountpoints.is_empty());
        assert!(device.label.is_none());
    }

    #[test]
    fn mountpoints_are_deduplicated_preserving_order() {
        let json = r#"{"blockdevices":[{"name":"sdb1","size":"8G","type":"part","fstype":"ext4","mountpoint":"/mnt/b","mountpoints":["/mnt/b","","/mnt/a","/mnt/b",null]}]}"#;
        let device = &parse_devices(json.as_bytes()).unwrap()[0];
        assert_eq!(
            device.mountpoints,
            vec!["/mnt/b".to_string(), "/mnt/a".to_string()]
        );
    }

    #[test]
    fn children_mountpoints_are_converted_recursively() {
        let json = r#"{"blockdevices":[{"name":"sdb","size":"8G","type":"disk","fstype":null,"mountpoint":null,"mountpoints":[null],"tran":"usb","children":[{"name":"sdb1","size":"4G","type":"part","fstype":"ext4","mountpoint":"/mnt/a","mountpoints":["/mnt/a","/mnt/b"],"children":[{"name":"sdb1-crypt","size":"4G","type":"crypt","fstype":"ext4","mountpoint":"/mnt/c","mountpoints":["/mnt/c"],"tran":null}]}]}]}"#;
        let devices = parse_devices(json.as_bytes()).unwrap();
        let part = &devices[0].children.as_ref().unwrap()[0];
        assert_eq!(part.mountpoints.len(), 2);
        assert_eq!(part.tran.as_deref(), Some("usb"));
        let crypt = &part.children.as_ref().unwrap()[0];
        assert_eq!(crypt.mountpoints, vec!["/mnt/c".to_string()]);
        assert_eq!(crypt.tran.as_deref(), Some("usb"));
    }

    #[test]
    fn any_mountpoint_makes_a_system_mount_protected() {
        let device = with_mounts(
            bd("sdb1", "part", Some("ext4"), None),
            &["/mnt/data", "/home/user"],
        );
        assert!(is_system_mount(&device));
        let device = with_mounts(
            bd("sdb2", "part", Some("ext4"), None),
            &["/mnt/data", "/media/u/USB"],
        );
        assert!(!is_system_mount(&device));
    }

    const LEGACY_JSON: &str = r#"{"blockdevices":[{"name":"legacy","size":"64G","type":"disk","fstype":null,"mountpoint":null,"label":null,"model":"LEGACY","vendor":null,"tran":"sata","serial":null,"uuid":null,"ro":false,"rm":false,"children":[{"name":"legacy1","size":"64G","type":"part","fstype":"ext4","mountpoint":"/legacy/mnt","label":"LEGACY","model":null,"vendor":null,"tran":null,"serial":null,"uuid":null,"ro":true,"rm":true}]}]}"#;

    #[test]
    fn preferred_columns_win_without_a_retry() {
        let runner = FakeRunner::new().push_ok(LEGACY_JSON);
        let devices = get_block_devices(&runner).unwrap();
        assert_eq!(devices[0].name, "legacy");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "lsblk");
        assert!(calls[0].args.iter().any(|arg| arg.contains("MOUNTPOINTS")));
    }

    #[test]
    fn fails_over_to_legacy_columns_when_mountpoints_is_rejected() {
        let runner = FakeRunner::new()
            .push_fail_with(32, "lsblk: unknown column: MOUNTPOINTS")
            .push_ok(LEGACY_JSON);

        let devices = get_block_devices(&runner).unwrap();
        assert_eq!(devices[0].name, "legacy");
        assert_eq!(
            devices[0].children.as_ref().unwrap()[0].mountpoints,
            vec!["/legacy/mnt".to_string()]
        );

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        // The retry drops only MOUNTPOINTS; the two specs must differ.
        assert_ne!(calls[0], calls[1]);
        assert!(calls[0].args.iter().any(|arg| arg.contains("MOUNTPOINTS")));
        assert!(!calls[1].args.iter().any(|arg| arg.contains("MOUNTPOINTS")));
        assert_eq!(calls[1].args[1], LSBLK_COLUMNS_LEGACY);
    }

    #[test]
    fn double_failure_reports_both_attempts() {
        let runner = FakeRunner::new()
            .push_fail_with(32, "first stderr: unknown column")
            .push_fail_with(1, "second stderr: no devices");

        let error = get_block_devices(&runner).unwrap_err().to_string();
        assert!(error.contains("first stderr: unknown column"), "{}", error);
        assert!(error.contains("second stderr: no devices"), "{}", error);
        assert!(error.contains("with MOUNTPOINTS"), "{}", error);
        assert!(error.contains("without it"), "{}", error);
        assert_eq!(runner.call_count(), 2);
    }

    #[test]
    fn double_failure_without_stderr_reports_exit_codes() {
        let runner = FakeRunner::new()
            .push_fail_with(32, "")
            .push_fail_with(1, "");

        let error = get_block_devices(&runner).unwrap_err().to_string();
        assert!(error.contains("exit code 32"), "{}", error);
        assert!(error.contains("exit code 1"), "{}", error);
    }

    #[test]
    fn spawn_error_is_propagated() {
        let runner = FakeRunner::new().push_err("lsblk not found");
        let error = get_block_devices(&runner).unwrap_err().to_string();
        assert!(error.contains("lsblk not found"), "{}", error);
        assert_eq!(runner.call_count(), 1);
    }
}
