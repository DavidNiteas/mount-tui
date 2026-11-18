//! Command line interface: subcommands, device lookup, output and exit codes.
//!
//! The CLI reuses the same configuration precedence, device tree, mount-target
//! policy and safety checks as the TUI. It never initialises the terminal: no
//! raw mode, no alternate screen, and the TUI main loop is never entered.
//!
//! Everything except [`main`]'s dispatch is a pure function of its inputs
//! (device list, configuration, injected runners, writers), so output text and
//! exit codes are unit-testable without touching a real device or `sudo`.

use std::fmt;
use std::io::{self, Write};
use std::process::Command as ProcessCommand;

use serde::Serialize;

use crate::app::parse_udisks_mountpoint;
use crate::config::{Config, MountMode, Overrides};
use crate::disk::{self, BlockDevice};
use crate::mount;
use crate::runner::{CommandRunner, CommandSpec};

/// Exit code: the command succeeded.
pub const EXIT_OK: i32 = 0;
/// Exit code: the requested operation failed.
pub const EXIT_FAILURE: i32 = 1;
/// Exit code: usage or configuration error.
pub const EXIT_USAGE: i32 = 2;

/// One CLI operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start the interactive TUI (no subcommand, or `tui`).
    Tui,
    /// `list [--json]`
    List { json: bool },
    /// `status <dev> [--json]`
    Status { device: String, json: bool },
    /// `mount <dev>`
    Mount { device: String },
    /// `unmount <dev>`
    Unmount { device: String },
    /// `force-unmount <dev> --yes`
    ForceUnmount { device: String },
}

/// A parsed command line that runs something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Command line configuration overrides (`--mount-mode` / `--mount-root`).
    pub overrides: Overrides,
    pub command: Command,
}

/// Result of parsing the command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Run the parsed command.
    Run(Invocation),
    /// `--help`: usage was printed, nothing runs.
    Help,
    /// `--version`: the version was printed, nothing runs.
    Version,
}

/// Parse the command line. `Err` carries a user-facing usage error (exit 2).
///
/// Global options (`--mount-mode`, `--mount-root`, `--help`, `--version`) are
/// accepted before and after the subcommand.
pub fn parse<I, S>(args: I) -> Result<Parsed, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut overrides = Overrides::default();
    let mut json = false;
    let mut yes = false;
    let mut name: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" | "-V" => return Ok(Parsed::Version),
            "--json" => {
                json = true;
                index += 1;
            }
            "--yes" | "-y" => {
                yes = true;
                index += 1;
            }
            "--mount-mode" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--mount-mode needs a value".to_string())?;
                overrides.mode = Some(value.clone());
                index += 2;
            }
            "--mount-root" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--mount-root needs a value".to_string())?;
                overrides.root = Some(value.clone());
                index += 2;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--mount-mode=") {
                    overrides.mode = Some(value.to_string());
                } else if let Some(value) = arg.strip_prefix("--mount-root=") {
                    overrides.root = Some(value.to_string());
                } else if arg.starts_with('-') {
                    return Err(format!("unknown option '{}'", arg));
                } else if name.is_none() {
                    name = Some(arg.to_string());
                } else {
                    positional.push(arg.to_string());
                }
                index += 1;
            }
        }
    }

    let command = match name.as_deref() {
        None => {
            reject_flag(json, "--json is only valid for 'list' and 'status'")?;
            reject_flag(yes, "--yes is only valid for 'force-unmount'")?;
            Command::Tui
        }
        Some("tui") => {
            reject_flag(json, "--json is only valid for 'list' and 'status'")?;
            reject_flag(yes, "--yes is only valid for 'force-unmount'")?;
            reject_extra("tui", &positional)?;
            Command::Tui
        }
        Some("list") => {
            reject_flag(yes, "--yes is only valid for 'force-unmount'")?;
            reject_extra("list", &positional)?;
            Command::List { json }
        }
        Some("status") => {
            reject_flag(yes, "--yes is only valid for 'force-unmount'")?;
            Command::Status {
                device: one_device("status", &positional)?,
                json,
            }
        }
        Some("mount") => {
            reject_flag(json, "--json is only valid for 'list' and 'status'")?;
            reject_flag(yes, "--yes is only valid for 'force-unmount'")?;
            Command::Mount {
                device: one_device("mount", &positional)?,
            }
        }
        Some("unmount") => {
            reject_flag(json, "--json is only valid for 'list' and 'status'")?;
            reject_flag(yes, "--yes is only valid for 'force-unmount'")?;
            Command::Unmount {
                device: one_device("unmount", &positional)?,
            }
        }
        Some("force-unmount") => {
            reject_flag(json, "--json is only valid for 'list' and 'status'")?;
            let device = one_device("force-unmount", &positional)?;
            if !yes {
                // A lazy unmount can lose data: it is never implicit.
                return Err("force-unmount needs --yes (a lazy unmount may lose data)".to_string());
            }
            Command::ForceUnmount { device }
        }
        Some(other) => return Err(format!("unknown command '{}'", other)),
    };

    Ok(Parsed::Run(Invocation { overrides, command }))
}

fn reject_flag(set: bool, message: &str) -> Result<(), String> {
    if set {
        return Err(message.to_string());
    }
    Ok(())
}

fn reject_extra(command: &str, positional: &[String]) -> Result<(), String> {
    if let Some(extra) = positional.first() {
        return Err(format!("'{}' takes no arguments, got '{}'", command, extra));
    }
    Ok(())
}

fn one_device(command: &str, positional: &[String]) -> Result<String, String> {
    let device = match positional {
        [] => {
            return Err(format!(
                "'{}' needs a device (name, /dev/<name> or label)",
                command
            ))
        }
        [device] => device.trim(),
        _ => {
            return Err(format!(
                "'{}' takes exactly one device, got {}",
                command,
                positional.len()
            ))
        }
    };
    if device.is_empty() {
        return Err(format!("'{}' needs a device", command));
    }
    Ok(device.to_string())
}

/// `<dev>` could not be resolved to exactly one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    NotFound {
        query: String,
        available: Vec<String>,
    },
    Ambiguous {
        query: String,
        matches: Vec<String>,
    },
}

impl fmt::Display for DeviceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { query, available } => {
                if available.is_empty() {
                    write!(
                        formatter,
                        "device '{}' not found (no block devices reported)",
                        query
                    )
                } else {
                    write!(
                        formatter,
                        "device '{}' not found; available: {}",
                        query,
                        available.join(", ")
                    )
                }
            }
            Self::Ambiguous { query, matches } => write!(
                formatter,
                "device '{}' is ambiguous; matches: {}",
                query,
                matches.join(", ")
            ),
        }
    }
}

/// Resolve `<dev>` over the whole tree.
///
/// `sdb1` and `/dev/sdb1` match the device name, a label matches
/// case-insensitively. Exactly one device must match; zero or several are
/// reported as errors (exit 2).
pub fn resolve_device<'a>(
    devices: &'a [BlockDevice],
    query: &str,
) -> Result<&'a BlockDevice, DeviceError> {
    let query = query.trim();
    let name = query.strip_prefix("/dev/").unwrap_or(query);
    let label = query.to_lowercase();

    let mut matches: Vec<&BlockDevice> = Vec::new();
    let mut available: Vec<String> = Vec::new();
    collect(devices, &mut available, &mut |device| {
        let by_name = !name.is_empty() && device.name == name;
        let by_label = device
            .label
            .as_deref()
            .is_some_and(|device_label| device_label.trim().to_lowercase() == label)
            && !query.is_empty();
        if by_name || by_label {
            matches.push(device);
        }
    });

    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(DeviceError::NotFound {
            query: query.to_string(),
            available,
        }),
        _ => Err(DeviceError::Ambiguous {
            query: query.to_string(),
            matches: matches.iter().map(|device| describe(device)).collect(),
        }),
    }
}

/// Walk the tree depth-first; `visit` sees every device, `names` collects names.
fn collect<'a>(
    devices: &'a [BlockDevice],
    names: &mut Vec<String>,
    visit: &mut impl FnMut(&'a BlockDevice),
) {
    for device in devices {
        names.push(device.name.clone());
        visit(device);
        if let Some(children) = &device.children {
            collect(children, names, visit);
        }
    }
}

fn describe(device: &BlockDevice) -> String {
    match device.label.as_deref() {
        Some(label) => format!("{} (/dev/{}, label '{}')", device.name, device.name, label),
        None => format!("{} (/dev/{})", device.name, device.name),
    }
}

fn find_device<'a>(devices: &'a [BlockDevice], name: &str) -> Option<&'a BlockDevice> {
    for device in devices {
        if device.name == name {
            return Some(device);
        }
        if let Some(children) = &device.children {
            if let Some(found) = find_device(children, name) {
                return Some(found);
            }
        }
    }
    None
}

/// Stable JSON projection of a device (the internal type stays private).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceJson {
    pub name: String,
    pub size: String,
    #[serde(rename = "type")]
    pub device_type: String,
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub tran: Option<String>,
    pub serial: Option<String>,
    pub mountpoints: Vec<String>,
    pub ro: bool,
    pub rm: bool,
    pub usb: bool,
    pub mounted: bool,
    pub mountable: bool,
    pub protected: bool,
}

impl DeviceJson {
    fn new(device: &BlockDevice) -> Self {
        Self {
            name: device.name.clone(),
            size: device.size.clone(),
            device_type: device.device_type.clone(),
            fstype: device.fstype.clone(),
            label: device.label.clone(),
            uuid: device.uuid.clone(),
            model: device.model.clone(),
            vendor: device.vendor.clone(),
            tran: device.tran.clone(),
            serial: device.serial.clone(),
            mountpoints: device.mountpoints.clone(),
            ro: device.ro,
            rm: device.rm,
            usb: disk::is_usb(device),
            mounted: disk::is_mounted(device),
            mountable: disk::is_mountable(device),
            protected: disk::is_system_mount(device),
        }
    }
}

/// One `list --json` node: device fields plus its children.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TreeNodeJson {
    #[serde(flatten)]
    pub device: DeviceJson,
    pub children: Vec<TreeNodeJson>,
}

impl TreeNodeJson {
    fn new(device: &BlockDevice) -> Self {
        Self {
            device: DeviceJson::new(device),
            children: device
                .children
                .iter()
                .flatten()
                .map(TreeNodeJson::new)
                .collect(),
        }
    }
}

/// `status --json` object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusJson {
    pub path: String,
    #[serde(flatten)]
    pub device: DeviceJson,
    pub multi_mount: bool,
}

/// Runs a command that needs the real terminal (the `sudo` password prompt).
///
/// It is deliberately separate from [`CommandRunner`]: interactive commands
/// inherit stdio and cannot be captured.
pub trait InteractiveRunner {
    /// Exit code of the finished command; `None` when it was killed by a signal.
    fn status(&self, program: &str, args: &[String]) -> io::Result<Option<i32>>;
}

/// Production runner: inherits stdin/stdout/stderr so `sudo` can prompt.
pub struct SystemInteractiveRunner;

impl InteractiveRunner for SystemInteractiveRunner {
    fn status(&self, program: &str, args: &[String]) -> io::Result<Option<i32>> {
        let status = ProcessCommand::new(program).args(args).status()?;
        Ok(status.code())
    }
}

/// Execute one CLI command; returns the process exit code.
///
/// `out` receives the command output, `err` receives failures. Nothing here
/// touches terminal state.
pub fn run(
    config: &Config,
    command: &Command,
    runner: &dyn CommandRunner,
    interactive: &dyn InteractiveRunner,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match command {
        // `main` handles the TUI itself; nothing to do here.
        Command::Tui => EXIT_OK,
        Command::List { json } => list(runner, *json, out, err),
        Command::Status { device, json } => status(runner, device, *json, out, err),
        Command::Mount { device } => mount(config, device, runner, interactive, out, err),
        Command::Unmount { device } => unmount(config, device, runner, interactive, out, err),
        Command::ForceUnmount { device } => {
            force_unmount(config, device, runner, interactive, out, err)
        }
    }
}

fn load_devices(runner: &dyn CommandRunner, err: &mut dyn Write) -> Result<Vec<BlockDevice>, i32> {
    match disk::get_block_devices(runner) {
        Ok(devices) => Ok(devices),
        Err(error) => {
            fail(err, format!("failed to list block devices: {:#}", error));
            Err(EXIT_FAILURE)
        }
    }
}

fn resolve<'a>(
    devices: &'a [BlockDevice],
    query: &str,
    err: &mut dyn Write,
) -> Result<&'a BlockDevice, i32> {
    match resolve_device(devices, query) {
        Ok(device) => Ok(device),
        Err(error) => {
            fail(err, error);
            Err(EXIT_USAGE)
        }
    }
}

fn list(runner: &dyn CommandRunner, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let devices = match load_devices(runner, err) {
        Ok(devices) => devices,
        Err(code) => return code,
    };

    if json {
        let tree: Vec<TreeNodeJson> = devices.iter().map(TreeNodeJson::new).collect();
        return write_json(out, err, &tree);
    }

    write_tree(out, &devices, 0);
    EXIT_OK
}

/// Write one JSON document followed by a newline.
///
/// A reader that closes the pipe early (`mount-tui list --json | head`) is not
/// an error: the command did its job.
fn write_json(out: &mut dyn Write, err: &mut dyn Write, value: &impl Serialize) -> i32 {
    let bytes = match serde_json::to_vec_pretty(value) {
        Ok(bytes) => bytes,
        Err(error) => {
            fail(err, format!("failed to write JSON: {}", error));
            return EXIT_FAILURE;
        }
    };
    match out.write_all(&bytes).and_then(|()| out.write_all(b"\n")) {
        Ok(()) => EXIT_OK,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => EXIT_OK,
        Err(error) => {
            fail(err, format!("failed to write JSON: {}", error));
            EXIT_FAILURE
        }
    }
}

fn write_tree(out: &mut dyn Write, devices: &[BlockDevice], depth: usize) {
    for device in devices {
        let _ = writeln!(out, "{}{}", "  ".repeat(depth), describe_line(device));
        if let Some(children) = &device.children {
            write_tree(out, children, depth + 1);
        }
    }
}

/// One human readable `list` line:
/// `<name> <size> <type> <fstype> <label> <mountpoints> [sys] [usb] [ro] [rm]`
fn describe_line(device: &BlockDevice) -> String {
    let mountpoints = if device.mountpoints.is_empty() {
        "-".to_string()
    } else {
        device.mountpoints.join(",")
    };
    let mut line = format!(
        "{} {} {} {} {} {}",
        device.name,
        device.size,
        device.device_type,
        device.fstype.as_deref().unwrap_or("-"),
        device.label.as_deref().unwrap_or("-"),
        mountpoints
    );
    let mut flags: Vec<&str> = Vec::new();
    if disk::is_system_mount(device) {
        flags.push("[sys]");
    }
    if disk::is_usb(device) {
        flags.push("[usb]");
    }
    if device.ro {
        flags.push("[ro]");
    }
    if device.rm {
        flags.push("[rm]");
    }
    if !flags.is_empty() {
        line.push(' ');
        line.push_str(&flags.join(" "));
    }
    line
}

fn status(
    runner: &dyn CommandRunner,
    query: &str,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let devices = match load_devices(runner, err) {
        Ok(devices) => devices,
        Err(code) => return code,
    };
    let device = match resolve(&devices, query, err) {
        Ok(device) => device,
        Err(code) => return code,
    };

    if json {
        let status = StatusJson {
            path: disk::device_path(&device.name),
            device: DeviceJson::new(device),
            multi_mount: device.mountpoints.len() > 1,
        };
        return write_json(out, err, &status);
    }

    let mountpoints = if device.mountpoints.is_empty() {
        "-".to_string()
    } else {
        device.mountpoints.join(", ")
    };
    let _ = writeln!(out, "path: {}", disk::device_path(&device.name));
    let _ = writeln!(out, "name: {}", device.name);
    let _ = writeln!(out, "size: {}", device.size);
    let _ = writeln!(out, "type: {}", device.device_type);
    let _ = writeln!(out, "fstype: {}", device.fstype.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "label: {}", device.label.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "uuid: {}", device.uuid.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "model: {}", device.model.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "vendor: {}", device.vendor.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "transport: {}", device.tran.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "serial: {}", device.serial.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "read_only: {}", device.ro);
    let _ = writeln!(out, "removable: {}", device.rm);
    let _ = writeln!(out, "usb: {}", disk::is_usb(device));
    let _ = writeln!(out, "mountpoints: {}", mountpoints);
    let _ = writeln!(out, "mounted: {}", disk::is_mounted(device));
    let _ = writeln!(out, "mountable: {}", disk::is_mountable(device));
    let _ = writeln!(out, "protected: {}", disk::is_system_mount(device));
    let _ = writeln!(out, "multi_mount: {}", device.mountpoints.len() > 1);
    EXIT_OK
}

/// Reasons every device operation refuses to run, mirroring the TUI checks.
fn mount_refusal(device: &BlockDevice) -> Option<String> {
    if !disk::is_mountable(device) {
        return Some(format!("{} cannot be mounted (no filesystem)", device.name));
    }
    if disk::is_mounted(device) {
        return Some(format!(
            "{} is already mounted at {}",
            device.name,
            device.mountpoints.join(", ")
        ));
    }
    None
}

fn unmount_refusal(device: &BlockDevice) -> Option<String> {
    if !disk::is_mounted(device) {
        return Some(format!("{} is not mounted", device.name));
    }
    if disk::is_system_mount(device) {
        return Some(format!(
            "Refusing to unmount {}: system mount",
            device.mountpoints.join(", ")
        ));
    }
    // Multiple mount points (bind mounts, ...) can only be detached by hand.
    if device.mountpoints.len() > 1 {
        return Some(format!(
            "{} has {} mount points; unmount manually (e.g. sudo umount <mp>)",
            device.name,
            device.mountpoints.len()
        ));
    }
    None
}

fn mount(
    config: &Config,
    query: &str,
    runner: &dyn CommandRunner,
    interactive: &dyn InteractiveRunner,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let devices = match load_devices(runner, err) {
        Ok(devices) => devices,
        Err(code) => return code,
    };
    let device = match resolve(&devices, query, err) {
        Ok(device) => device,
        Err(code) => return code,
    };
    if let Some(reason) = mount_refusal(device) {
        fail(err, reason);
        return EXIT_FAILURE;
    }

    match config.mount_mode {
        MountMode::Udisksctl => mount_udisksctl(runner, device, out, err),
        MountMode::Sudo => mount_sudo(config, runner, interactive, device, out, err),
    }
}

/// Unprivileged mount: udisks2 picks the mount point itself.
fn mount_udisksctl(
    runner: &dyn CommandRunner,
    device: &BlockDevice,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let dev_path = disk::device_path(&device.name);
    match runner.output(&CommandSpec::new(
        "udisksctl",
        &["mount", "-b", dev_path.as_str()],
    )) {
        Ok(output) if output.success => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let mount_point =
                mounted_at(runner, &device.name).or_else(|| parse_udisks_mountpoint(&stdout));
            match mount_point {
                Some(mount_point) => {
                    let _ = writeln!(out, "Mounted {} at {}", dev_path, mount_point);
                }
                None => {
                    let _ = writeln!(out, "Mounted {}", dev_path);
                }
            }
            EXIT_OK
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            fail(err, mount_failed(&dev_path, &stderr));
            EXIT_FAILURE
        }
        Err(error) => {
            fail(
                err,
                mount_failed(&dev_path, &format!("udisksctl error: {}", error)),
            );
            EXIT_FAILURE
        }
    }
}

/// `sudo mount` into `root/<label-or-name>`; the target is created here and
/// removed again (only when this call created it) if the command did not
/// succeed.
fn mount_sudo(
    config: &Config,
    runner: &dyn CommandRunner,
    interactive: &dyn InteractiveRunner,
    device: &BlockDevice,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let dev_path = disk::device_path(&device.name);
    let target = match mount::resolve_target(&config.mount_root, device) {
        Ok(target) => target,
        Err(error) => {
            fail(err, format!("Cannot mount {}: {}", dev_path, error));
            return EXIT_FAILURE;
        }
    };
    let options = mount::mount_options(device, mount::home_owner());
    let created = match mount::prepare_mount_target(Some(&target)) {
        Ok(created) => created,
        Err(message) => {
            fail(err, message);
            return EXIT_FAILURE;
        }
    };
    let mut cleanup = mount::CreatedDirGuard::new(created);

    let args = vec![
        "mount".to_string(),
        "-o".to_string(),
        options,
        dev_path.clone(),
        target.display().to_string(),
    ];
    match interactive.status("sudo", &args) {
        Ok(Some(0)) => {
            // A successful mount keeps the (possibly newly created) directory.
            cleanup.disarm();
            let mount_point =
                mounted_at(runner, &device.name).unwrap_or_else(|| target.display().to_string());
            let _ = writeln!(out, "Mounted {} at {}", dev_path, mount_point);
            EXIT_OK
        }
        Ok(Some(code)) => {
            drop(cleanup);
            fail(
                err,
                mount_failed(
                    &dev_path,
                    &format!("sudo mount exited with status {}", code),
                ),
            );
            EXIT_FAILURE
        }
        Ok(None) => {
            drop(cleanup);
            fail(
                err,
                mount_failed(&dev_path, "sudo mount was terminated by a signal"),
            );
            EXIT_FAILURE
        }
        Err(error) => {
            drop(cleanup);
            fail(
                err,
                mount_failed(&dev_path, &format!("failed to run sudo: {}", error)),
            );
            EXIT_FAILURE
        }
    }
}

fn unmount(
    config: &Config,
    query: &str,
    runner: &dyn CommandRunner,
    interactive: &dyn InteractiveRunner,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let devices = match load_devices(runner, err) {
        Ok(devices) => devices,
        Err(code) => return code,
    };
    let device = match resolve(&devices, query, err) {
        Ok(device) => device,
        Err(code) => return code,
    };
    if let Some(reason) = unmount_refusal(device) {
        fail(err, reason);
        return EXIT_FAILURE;
    }
    let dev_path = disk::device_path(&device.name);

    match config.mount_mode {
        MountMode::Udisksctl => {
            match runner.output(&CommandSpec::new(
                "udisksctl",
                &["unmount", "-b", dev_path.as_str()],
            )) {
                Ok(output) if output.success => {
                    let _ = writeln!(out, "Unmounted {}", dev_path);
                    EXIT_OK
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    fail(err, unmount_failed(&dev_path, &stderr));
                    EXIT_FAILURE
                }
                Err(error) => {
                    fail(
                        err,
                        unmount_failed(&dev_path, &format!("udisksctl error: {}", error)),
                    );
                    EXIT_FAILURE
                }
            }
        }
        MountMode::Sudo => {
            let args = vec!["umount".to_string(), dev_path.clone()];
            match interactive.status("sudo", &args) {
                Ok(Some(0)) => {
                    let _ = writeln!(out, "Unmounted {}", dev_path);
                    EXIT_OK
                }
                Ok(Some(code)) => {
                    fail(
                        err,
                        unmount_failed(
                            &dev_path,
                            &format!("sudo umount exited with status {}", code),
                        ),
                    );
                    EXIT_FAILURE
                }
                Ok(None) => {
                    fail(
                        err,
                        unmount_failed(&dev_path, "sudo umount was terminated by a signal"),
                    );
                    EXIT_FAILURE
                }
                Err(error) => {
                    fail(
                        err,
                        unmount_failed(&dev_path, &format!("failed to run sudo: {}", error)),
                    );
                    EXIT_FAILURE
                }
            }
        }
    }
}

fn force_unmount(
    config: &Config,
    query: &str,
    runner: &dyn CommandRunner,
    interactive: &dyn InteractiveRunner,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let devices = match load_devices(runner, err) {
        Ok(devices) => devices,
        Err(code) => return code,
    };
    let device = match resolve(&devices, query, err) {
        Ok(device) => device,
        Err(code) => return code,
    };
    if let Some(reason) = unmount_refusal(device) {
        fail(err, reason);
        return EXIT_FAILURE;
    }
    let dev_path = disk::device_path(&device.name);

    match config.mount_mode {
        MountMode::Udisksctl => {
            match runner.output(&CommandSpec::new(
                "udisksctl",
                &["unmount", "-b", dev_path.as_str(), "-f"],
            )) {
                Ok(output) if output.success => {
                    force_unmounted(out, &dev_path);
                    EXIT_OK
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    fail(err, force_unmount_failed(&dev_path, &stderr));
                    EXIT_FAILURE
                }
                Err(error) => {
                    fail(
                        err,
                        force_unmount_failed(&dev_path, &format!("udisksctl error: {}", error)),
                    );
                    EXIT_FAILURE
                }
            }
        }
        MountMode::Sudo => {
            // Same attempts as the TUI: the device path first, its mount point
            // as the fallback.
            let mut attempts: Vec<Vec<String>> = vec![vec![
                "umount".to_string(),
                "-l".to_string(),
                dev_path.clone(),
            ]];
            if let Some(mount_point) = device.mountpoints.first() {
                attempts.push(vec![
                    "umount".to_string(),
                    "-l".to_string(),
                    mount_point.clone(),
                ]);
            }
            let mut last = String::from("sudo umount did not run");
            for args in attempts {
                match interactive.status("sudo", &args) {
                    Ok(Some(0)) => {
                        force_unmounted(out, &dev_path);
                        return EXIT_OK;
                    }
                    Ok(Some(code)) => {
                        last = format!("sudo umount -l exited with status {}", code);
                    }
                    Ok(None) => {
                        last = "sudo umount -l was terminated by a signal".to_string();
                    }
                    Err(error) => {
                        last = format!("failed to run sudo: {}", error);
                    }
                }
            }
            fail(err, force_unmount_failed(&dev_path, &last));
            EXIT_FAILURE
        }
    }
}

fn force_unmounted(out: &mut dyn Write, dev_path: &str) {
    let _ = writeln!(
        out,
        "Force unmounted {} (lazy: detached, cleaned up when no longer busy)",
        dev_path
    );
}

/// First mount point reported for the device after a re-read, if any.
fn mounted_at(runner: &dyn CommandRunner, device_name: &str) -> Option<String> {
    let devices = disk::get_block_devices(runner).ok()?;
    find_device(&devices, device_name).and_then(|device| device.mountpoints.first().cloned())
}

fn mount_failed(dev_path: &str, context: &str) -> String {
    format!("Mount failed for {}: {}", dev_path, context)
}

fn unmount_failed(dev_path: &str, context: &str) -> String {
    format!("Unmount failed for {}: {}", dev_path, context)
}

fn force_unmount_failed(dev_path: &str, context: &str) -> String {
    format!("Force unmount failed for {}: {}", dev_path, context)
}

/// Report a failure on stderr; the exit code is chosen by the caller.
fn fail(err: &mut dyn Write, message: impl fmt::Display) {
    let _ = writeln!(err, "mount-tui: {}", message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::FakeRunner;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TREE_JSON: &str = r#"{"blockdevices":[
        {"name":"sda","size":"100G","type":"disk","fstype":null,"mountpoints":[null],"label":null,"model":"SYSDISK","vendor":"V","tran":"sata","serial":"S1","uuid":null,"ro":false,"rm":false,"children":[
            {"name":"sda1","size":"100G","type":"part","fstype":"ext4","mountpoints":["/home"],"label":null,"model":null,"vendor":null,"tran":null,"serial":null,"uuid":"U1","ro":false,"rm":false}
        ]},
        {"name":"sdb","size":"8G","type":"disk","fstype":null,"mountpoints":[null],"label":null,"model":"USBSTICK","vendor":"Generic","tran":"usb","serial":"S2","uuid":null,"ro":false,"rm":true,"children":[
            {"name":"sdb1","size":"8G","type":"part","fstype":"vfat","mountpoints":[null],"label":"USBKEY","model":null,"vendor":null,"tran":null,"serial":null,"uuid":"ABCD","ro":false,"rm":true},
            {"name":"sdb2","size":"4G","type":"part","fstype":"ext4","mountpoints":[null],"label":"BACKUP","model":null,"vendor":null,"tran":null,"serial":null,"uuid":"EFGH","ro":false,"rm":false}
        ]},
        {"name":"sdc","size":"16G","type":"disk","fstype":null,"mountpoints":[null],"label":null,"model":null,"vendor":null,"tran":"sata","serial":null,"uuid":null,"ro":false,"rm":false,"children":[
            {"name":"sdc1","size":"16G","type":"part","fstype":"ext4","mountpoints":[null],"label":"DUP","model":null,"vendor":null,"tran":null,"serial":null,"uuid":null,"ro":false,"rm":false}
        ]},
        {"name":"sdd","size":"16G","type":"disk","fstype":null,"mountpoints":[null],"label":null,"model":null,"vendor":null,"tran":"sata","serial":null,"uuid":null,"ro":false,"rm":false,"children":[
            {"name":"sdd1","size":"16G","type":"part","fstype":"ext4","mountpoints":[null],"label":"dup","model":null,"vendor":null,"tran":null,"serial":null,"uuid":null,"ro":false,"rm":false}
        ]},
        {"name":"sdy","size":"1T","type":"disk","fstype":null,"mountpoints":[null],"label":null,"model":"MULTI","vendor":null,"tran":"sata","serial":null,"uuid":null,"ro":false,"rm":false,"children":[
            {"name":"sdy1","size":"1T","type":"part","fstype":"ext4","mountpoints":["/mnt/a","/mnt/b"],"label":"MULTI","model":null,"vendor":null,"tran":null,"serial":null,"uuid":null,"ro":false,"rm":false}
        ]},
        {"name":"sdz","size":"2G","type":"disk","fstype":null,"mountpoints":[null],"label":null,"model":null,"vendor":null,"tran":"sata","serial":null,"uuid":null,"ro":false,"rm":false,"children":[
            {"name":"sdz1","size":"2G","type":"part","fstype":"ext4","mountpoints":["/mnt/data"],"label":"DATA","model":null,"vendor":null,"tran":null,"serial":null,"uuid":null,"ro":true,"rm":false}
        ]},
        {"name":"sdm","size":"1G","type":"part","fstype":"ext4","mountpoints":[null],"label":null,"model":null,"vendor":null,"tran":"usb","serial":null,"uuid":null,"ro":false,"rm":true}
    ]}"#;

    fn tree() -> Vec<BlockDevice> {
        disk::get_block_devices(&FakeRunner::new().push_ok(TREE_JSON)).unwrap()
    }

    fn devices_only() -> FakeRunner {
        FakeRunner::new().push_ok(TREE_JSON)
    }

    #[derive(Default)]
    struct FakeInteractive {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        results: RefCell<VecDeque<io::Result<Option<i32>>>>,
    }

    impl FakeInteractive {
        fn new() -> Self {
            Self::default()
        }

        fn push_code(self, code: i32) -> Self {
            self.results.borrow_mut().push_back(Ok(Some(code)));
            self
        }

        fn push_signal(self) -> Self {
            self.results.borrow_mut().push_back(Ok(None));
            self
        }

        fn push_err(self, message: &str) -> Self {
            self.results
                .borrow_mut()
                .push_back(Err(io::Error::new(io::ErrorKind::NotFound, message)));
            self
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.borrow().clone()
        }

        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl InteractiveRunner for FakeInteractive {
        fn status(&self, program: &str, args: &[String]) -> io::Result<Option<i32>> {
            self.calls
                .borrow_mut()
                .push((program.to_string(), args.to_vec()));
            match self.results.borrow_mut().pop_front() {
                Some(result) => result,
                None => panic!("FakeInteractive: unexpected command {} {:?}", program, args),
            }
        }
    }

    /// Unique scratch directory; removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "mount-tui-cli-{}-{}-{}",
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

    fn udisksctl_config() -> Config {
        Config {
            mount_mode: MountMode::Udisksctl,
            mount_root: PathBuf::from("/nonexistent/mount-tui-test-root"),
        }
    }

    fn sudo_config(root: &Path) -> Config {
        Config {
            mount_mode: MountMode::Sudo,
            mount_root: root.to_path_buf(),
        }
    }

    fn run_command(
        config: &Config,
        command: &Command,
        runner: &dyn CommandRunner,
        interactive: &dyn InteractiveRunner,
    ) -> (i32, String, String) {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = run(config, command, runner, interactive, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    // --- command line parsing ------------------------------------------------

    fn parsed(args: &[&str]) -> Invocation {
        match parse(args.iter().map(|arg| arg.to_string())).unwrap() {
            Parsed::Run(invocation) => invocation,
            other => panic!("expected a runnable invocation, got {:?}", other),
        }
    }

    fn overrides(mode: Option<&str>, root: Option<&str>) -> Overrides {
        Overrides {
            mode: mode.map(str::to_string),
            root: root.map(str::to_string),
        }
    }

    #[test]
    fn no_subcommand_starts_the_tui() {
        assert_eq!(parsed(&[]).command, Command::Tui);
        assert_eq!(parsed(&[]).overrides, Overrides::default());
        assert_eq!(parsed(&["tui"]).command, Command::Tui);
    }

    #[test]
    fn global_options_are_accepted_before_and_after_the_subcommand() {
        assert_eq!(
            parsed(&[
                "--mount-mode",
                "sudo",
                "--mount-root",
                "/srv/usb",
                "mount",
                "sdb1"
            ]),
            Invocation {
                overrides: overrides(Some("sudo"), Some("/srv/usb")),
                command: Command::Mount {
                    device: "sdb1".into()
                },
            }
        );
        assert_eq!(
            parsed(&["mount", "sdb1", "--mount-mode=sudo", "--mount-root=~/usb"]),
            Invocation {
                overrides: overrides(Some("sudo"), Some("~/usb")),
                command: Command::Mount {
                    device: "sdb1".into()
                },
            }
        );
        // Later values win, like every other option.
        assert_eq!(
            parsed(&["--mount-mode", "sudo", "--mount-mode", "udisksctl"]).overrides,
            overrides(Some("udisksctl"), None)
        );
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(["--help"]).unwrap(), Parsed::Help);
        assert_eq!(parse(["-h"]).unwrap(), Parsed::Help);
        assert_eq!(parse(["--version"]).unwrap(), Parsed::Version);
        assert_eq!(parse(["-V"]).unwrap(), Parsed::Version);
        // Also after a subcommand.
        assert_eq!(parse(["list", "--help"]).unwrap(), Parsed::Help);
    }

    #[test]
    fn every_subcommand_is_parsed() {
        assert_eq!(parsed(&["list"]).command, Command::List { json: false });
        assert_eq!(
            parsed(&["list", "--json"]).command,
            Command::List { json: true }
        );
        assert_eq!(
            parsed(&["status", "sdb1"]).command,
            Command::Status {
                device: "sdb1".into(),
                json: false
            }
        );
        assert_eq!(
            parsed(&["status", "/dev/sdb1", "--json"]).command,
            Command::Status {
                device: "/dev/sdb1".into(),
                json: true
            }
        );
        assert_eq!(
            parsed(&["mount", "sdb1"]).command,
            Command::Mount {
                device: "sdb1".into()
            }
        );
        assert_eq!(
            parsed(&["unmount", "sdb1"]).command,
            Command::Unmount {
                device: "sdb1".into()
            }
        );
        assert_eq!(
            parsed(&["force-unmount", "sdb1", "--yes"]).command,
            Command::ForceUnmount {
                device: "sdb1".into()
            }
        );
        assert_eq!(
            parsed(&["force-unmount", "-y", "sdb1"]).command,
            Command::ForceUnmount {
                device: "sdb1".into()
            }
        );
    }

    #[test]
    fn force_unmount_requires_yes() {
        let error = parse(["force-unmount", "sdb1"]).unwrap_err();
        assert!(error.contains("--yes"), "{}", error);
        assert!(error.contains("lose data"), "{}", error);
    }

    #[test]
    fn usage_errors_are_reported() {
        let cases: &[(&[&str], &str)] = &[
            (&["--nope"], "unknown option"),
            (&["--mount-mode"], "needs a value"),
            (&["--mount-root"], "needs a value"),
            (&["wipe", "sdb1"], "unknown command"),
            (&["status"], "needs a device"),
            (&["mount"], "needs a device"),
            (&["unmount"], "needs a device"),
            (&["force-unmount"], "needs a device"),
            (&["mount", "a", "b"], "exactly one device"),
            (&["list", "extra"], "takes no arguments"),
            (&["tui", "extra"], "takes no arguments"),
            (&["tui", "--json"], "--json"),
            (&["--json"], "--json"),
            (&["list", "--yes"], "--yes"),
            (&["mount", "sdb1", "--json"], "--json"),
            (&["unmount", "sdb1", "-y"], "--yes"),
            (&["force-unmount", "sdb1", "--json"], "--json"),
        ];
        for (args, expected) in cases {
            let error = parse(args.iter().map(|arg| arg.to_string())).unwrap_err();
            assert!(error.contains(expected), "{:?} -> {}", args, error);
        }
        // An empty device is refused as well.
        assert!(parse(["status", "  "])
            .unwrap_err()
            .contains("needs a device"));
    }

    // --- device lookup -------------------------------------------------------

    #[test]
    fn devices_are_found_by_name_path_and_label() {
        let devices = tree();
        assert_eq!(resolve_device(&devices, "sdb1").unwrap().name, "sdb1");
        assert_eq!(resolve_device(&devices, "sdb").unwrap().name, "sdb");
        assert_eq!(resolve_device(&devices, "/dev/sdb1").unwrap().name, "sdb1");
        assert_eq!(resolve_device(&devices, "USBKEY").unwrap().name, "sdb1");
        // Labels are matched case-insensitively and trimmed.
        assert_eq!(resolve_device(&devices, "usbkey").unwrap().name, "sdb1");
        assert_eq!(resolve_device(&devices, "  Backup  ").unwrap().name, "sdb2");
        // A nested device is reachable through its parent.
        assert_eq!(resolve_device(&devices, "sdz1").unwrap().name, "sdz1");
    }

    #[test]
    fn a_duplicated_label_is_ambiguous() {
        let devices = tree();
        let error = resolve_device(&devices, "DUP").unwrap_err();
        match error {
            DeviceError::Ambiguous { ref matches, .. } => {
                assert_eq!(matches.len(), 2, "{:?}", matches);
                assert!(matches[0].contains("sdc1"), "{:?}", matches);
                assert!(matches[1].contains("sdd1"), "{:?}", matches);
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
        let message = resolve_device(&devices, "dup").unwrap_err().to_string();
        assert!(message.contains("ambiguous"), "{}", message);
        assert!(message.contains("label 'DUP'"), "{}", message);
    }

    #[test]
    fn an_unknown_device_lists_the_available_names() {
        let devices = tree();
        let message = resolve_device(&devices, "sdq9").unwrap_err().to_string();
        assert!(message.contains("not found"), "{}", message);
        assert!(message.contains("sdb1"), "{}", message);
        // A path that does not exist is not found either.
        assert!(resolve_device(&devices, "/dev/sdq9").is_err());
        // An empty list reports that nothing was found.
        assert!(resolve_device(&[], "sdb1")
            .unwrap_err()
            .to_string()
            .contains("no block devices"));
    }

    // --- list / status output ------------------------------------------------

    #[test]
    fn list_prints_an_indented_tree_with_flags() {
        let (code, out, err) = run_command(
            &udisksctl_config(),
            &Command::List { json: false },
            &devices_only(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(err.is_empty(), "{}", err);
        assert!(out.contains("sdb 8G disk - - - [usb] [rm]"), "{}", out);
        assert!(
            out.contains("  sdb1 8G part vfat USBKEY - [usb] [rm]"),
            "{}",
            out
        );
        // The system mount is flagged, the nested line is indented by two spaces.
        assert!(out.contains("sda1 100G part ext4 - /home [sys]"), "{}", out);
        assert!(
            out.contains("sdy1 1T part ext4 MULTI /mnt/a,/mnt/b"),
            "{}",
            out
        );
    }

    #[test]
    fn list_json_is_stable_and_nested() {
        let (code, out, _) = run_command(
            &udisksctl_config(),
            &Command::List { json: true },
            &devices_only(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let devices = value.as_array().expect("array of devices");
        assert_eq!(devices.len(), 7, "{}", out);

        let sdb = &devices[1];
        assert_eq!(sdb["name"], "sdb");
        assert_eq!(sdb["type"], "disk");
        assert_eq!(sdb["usb"], true);
        assert_eq!(sdb["rm"], true);
        assert_eq!(sdb["mounted"], false);
        let children = sdb["children"].as_array().expect("children array");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0]["name"], "sdb1");
        assert_eq!(children[0]["label"], "USBKEY");
        assert_eq!(children[0]["mountable"], true);
        assert_eq!(children[0]["children"].as_array().unwrap().len(), 0);

        assert_eq!(devices[0]["children"][0]["protected"], true);
        // Fields stay in place even when their value is null.
        assert!(devices[0]["fstype"].is_null());
    }

    #[test]
    fn status_prints_fields_and_booleans() {
        let (code, out, _) = run_command(
            &udisksctl_config(),
            &Command::Status {
                device: "sdb1".into(),
                json: false,
            },
            &devices_only(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("path: /dev/sdb1"), "{}", out);
        assert!(out.contains("label: USBKEY"), "{}", out);
        assert!(out.contains("mounted: false"), "{}", out);
        assert!(out.contains("mountable: true"), "{}", out);
        assert!(out.contains("protected: false"), "{}", out);
        assert!(out.contains("multi_mount: false"), "{}", out);

        let (code, out, _) = run_command(
            &udisksctl_config(),
            &Command::Status {
                device: "sdy1".into(),
                json: false,
            },
            &devices_only(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("mountpoints: /mnt/a, /mnt/b"), "{}", out);
        assert!(out.contains("multi_mount: true"), "{}", out);
        assert!(out.contains("mounted: true"), "{}", out);
    }

    #[test]
    fn status_json_is_an_object() {
        let (code, out, _) = run_command(
            &udisksctl_config(),
            &Command::Status {
                device: "USBKEY".into(),
                json: true,
            },
            &devices_only(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(value.is_object(), "{}", out);
        assert_eq!(value["path"], "/dev/sdb1");
        assert_eq!(value["name"], "sdb1");
        assert_eq!(value["label"], "USBKEY");
        assert_eq!(value["mounted"], false);
        assert_eq!(value["mountable"], true);
        assert_eq!(value["protected"], false);
        assert_eq!(value["multi_mount"], false);
        assert_eq!(value["usb"], true);
    }

    #[test]
    fn an_unknown_device_exits_with_the_usage_code() {
        let (code, out, err) = run_command(
            &udisksctl_config(),
            &Command::Status {
                device: "sdq9".into(),
                json: false,
            },
            &devices_only(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_USAGE);
        assert!(out.is_empty(), "{}", out);
        assert!(err.contains("not found"), "{}", err);
        assert!(err.contains("available"), "{}", err);
    }

    #[test]
    fn a_failing_lsblk_is_an_operation_failure() {
        let runner = FakeRunner::new()
            .push_fail_with(32, "lsblk: unknown column")
            .push_fail_with(1, "lsblk: boom");
        let (code, out, err) = run_command(
            &udisksctl_config(),
            &Command::List { json: false },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(out.is_empty(), "{}", out);
        assert!(err.contains("failed to list block devices"), "{}", err);
    }

    // --- mount / unmount / force (udisksctl) ---------------------------------

    #[test]
    fn mount_refuses_a_device_without_a_filesystem() {
        let runner = devices_only();
        let interactive = FakeInteractive::new();
        let (code, out, err) = run_command(
            &udisksctl_config(),
            &Command::Mount {
                device: "sdb".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(out.is_empty(), "{}", out);
        assert!(err.contains("no filesystem"), "{}", err);
        // Only the lsblk read happened; nothing was mounted and nothing escalated.
        assert_eq!(runner.call_count(), 1);
        assert_eq!(interactive.call_count(), 0);
    }

    #[test]
    fn mount_refuses_an_already_mounted_device() {
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let (code, _, err) = run_command(
            &udisksctl_config(),
            &Command::Mount {
                device: "sda1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("already mounted at /home"), "{}", err);
        assert_eq!(runner.call_count(), 1);
    }

    #[test]
    fn udisksctl_mount_reports_the_real_mount_point() {
        // lsblk (initial) → udisksctl mount → lsblk (re-read, now mounted).
        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_ok("Mounted /dev/sdb1 at /media/e2e/USBKEY.")
            .push_ok(TREE_JSON);
        let interactive = FakeInteractive::new();
        let (code, out, err) = run_command(
            &udisksctl_config(),
            &Command::Mount {
                device: "sdb1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_OK, "{}", err);
        assert!(err.is_empty(), "{}", err);
        assert!(
            out.contains("Mounted /dev/sdb1 at /media/e2e/USBKEY"),
            "{}",
            out
        );
        let calls = runner.calls();
        assert_eq!(
            calls[1],
            CommandSpec::new("udisksctl", &["mount", "-b", "/dev/sdb1"])
        );
        // Default mode never touches sudo and never creates a directory.
        assert_eq!(interactive.call_count(), 0);
    }

    #[test]
    fn udisksctl_mount_falls_back_to_its_output() {
        // The re-read cannot report the device (empty tree), so the parsed
        // stdout of `udisksctl` is used.
        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_ok("Mounted /dev/sdb1 at /media/e2e/My Disk.")
            .push_ok(r#"{"blockdevices":[]}"#);
        let (code, out, _) = run_command(
            &udisksctl_config(),
            &Command::Mount {
                device: "sdb1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(
            out.contains("Mounted /dev/sdb1 at /media/e2e/My Disk"),
            "{}",
            out
        );
    }

    #[test]
    fn a_failing_udisksctl_mount_is_exit_one_without_sudo() {
        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_fail("not authorized");
        let interactive = FakeInteractive::new();
        let (code, out, err) = run_command(
            &udisksctl_config(),
            &Command::Mount {
                device: "sdb1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(out.is_empty(), "{}", out);
        assert!(err.contains("Mount failed for /dev/sdb1"), "{}", err);
        assert!(err.contains("not authorized"), "{}", err);
        assert_eq!(interactive.call_count(), 0, "no sudo escalation");
        assert_eq!(runner.call_count(), 2, "no retry");
    }

    #[test]
    fn udisksctl_mount_spawn_error_is_reported() {
        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_err("udisksctl not found");
        let (code, _, err) = run_command(
            &udisksctl_config(),
            &Command::Mount {
                device: "sdb1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(
            err.contains("udisksctl error: udisksctl not found"),
            "{}",
            err
        );
    }

    #[test]
    fn unmount_refuses_what_the_tui_refuses() {
        // Not mounted.
        let (code, _, err) = run_command(
            &udisksctl_config(),
            &Command::Unmount {
                device: "sdb1".into(),
            },
            &devices_only(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("is not mounted"), "{}", err);

        // System mount.
        let runner = devices_only();
        let (code, _, err) = run_command(
            &udisksctl_config(),
            &Command::Unmount {
                device: "sda1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("system mount"), "{}", err);
        assert_eq!(runner.call_count(), 1, "nothing ran after the refusal");

        // Multiple mount points.
        let runner = devices_only();
        let (code, _, err) = run_command(
            &udisksctl_config(),
            &Command::ForceUnmount {
                device: "sdy1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("has 2 mount points"), "{}", err);
        assert_eq!(runner.call_count(), 1);
    }

    #[test]
    fn udisksctl_unmount_and_force_unmount() {
        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_ok("Unmounted /dev/sdz1.");
        let (code, out, _) = run_command(
            &udisksctl_config(),
            &Command::Unmount {
                device: "sdz1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("Unmounted /dev/sdz1"), "{}", out);
        assert_eq!(
            runner.calls()[1],
            CommandSpec::new("udisksctl", &["unmount", "-b", "/dev/sdz1"])
        );

        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_ok("Unmounted /dev/sdz1.");
        let (code, out, _) = run_command(
            &udisksctl_config(),
            &Command::ForceUnmount {
                device: "sdz1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("Force unmounted /dev/sdz1"), "{}", out);
        assert_eq!(
            runner.calls()[1],
            CommandSpec::new("udisksctl", &["unmount", "-b", "/dev/sdz1", "-f"])
        );
    }

    #[test]
    fn a_failing_udisksctl_unmount_is_exit_one() {
        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_fail("target is busy");
        let (code, _, err) = run_command(
            &udisksctl_config(),
            &Command::Unmount {
                device: "sdz1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("Unmount failed for /dev/sdz1"), "{}", err);
        assert!(err.contains("target is busy"), "{}", err);
    }

    // --- sudo mode -----------------------------------------------------------

    #[test]
    fn sudo_mount_creates_the_target_and_keeps_it_on_success() {
        let root = TempDir::new("sudo-ok");
        let target = root.path().join("BACKUP");
        // lsblk (initial) → lsblk (re-read; the device is not reported as
        // mounted, so the planned target is printed).
        let runner = FakeRunner::new().push_ok(TREE_JSON).push_ok(TREE_JSON);
        let interactive = FakeInteractive::new().push_code(0);
        let (code, out, err) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sdb2".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_OK, "{}", err);
        assert!(target.is_dir(), "the mount point is created");
        assert!(
            out.contains(&format!("Mounted /dev/sdb2 at {}", target.display())),
            "{}",
            out
        );
        assert_eq!(
            interactive.calls(),
            vec![(
                "sudo".to_string(),
                vec![
                    "mount".to_string(),
                    "-o".to_string(),
                    "nosuid,nodev".to_string(),
                    "/dev/sdb2".to_string(),
                    target.display().to_string(),
                ]
            )]
        );
        // sudo mode never consults udisksctl.
        assert!(!runner
            .calls()
            .iter()
            .any(|call| call.program == "udisksctl"));
    }

    #[test]
    fn sudo_mount_uses_the_owner_options_for_single_owner_filesystems() {
        let root = TempDir::new("sudo-vfat");
        let runner = FakeRunner::new().push_ok(TREE_JSON).push_ok(TREE_JSON);
        let interactive = FakeInteractive::new().push_code(0);
        let (code, _, err) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sdb1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_OK, "{}", err);
        let (_, args) = interactive.calls().remove(0);
        assert_eq!(args[0], "mount");
        assert_eq!(args[1], "-o");
        assert!(
            args[2].starts_with("nosuid,nodev,uid="),
            "vfat gets the owner options: {}",
            args[2]
        );
        assert!(args[2].contains("gid="), "{}", args[2]);
        assert!(args[2].ends_with("umask=0022"), "{}", args[2]);
        assert_eq!(args[4], root.path().join("USBKEY").display().to_string());
        assert!(root.path().join("USBKEY").is_dir());
    }

    #[test]
    fn sudo_mount_reports_the_real_mount_point_after_a_re_read() {
        let root = TempDir::new("sudo-real");
        // The re-read reports sdb2 mounted; the real mount point must win over
        // the planned target.
        let runner = FakeRunner::new().push_ok(TREE_JSON).push_ok(
            r#"{"blockdevices":[{"name":"sdb","size":"8G","type":"disk","fstype":null,"mountpoints":[null],"label":null,"model":null,"vendor":null,"tran":"usb","serial":null,"uuid":null,"ro":false,"rm":true,"children":[{"name":"sdb2","size":"4G","type":"part","fstype":"ext4","mountpoints":["/srv/real"],"label":"BACKUP","model":null,"vendor":null,"tran":null,"serial":null,"uuid":null,"ro":false,"rm":false}]}]}"#,
        );
        let (code, out, _) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sdb2".into(),
            },
            &runner,
            &FakeInteractive::new().push_code(0),
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("Mounted /dev/sdb2 at /srv/real"), "{}", out);
    }

    #[test]
    fn a_failing_sudo_mount_removes_the_directory_it_created() {
        let root = TempDir::new("sudo-fail");
        let target = root.path().join("BACKUP");
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let (code, out, err) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sdb2".into(),
            },
            &runner,
            &FakeInteractive::new().push_code(1),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(out.is_empty(), "{}", out);
        assert!(err.contains("sudo mount exited with status 1"), "{}", err);
        assert!(!target.exists(), "the failed mount leaves no mount point");
    }

    #[test]
    fn a_cancelled_sudo_mount_removes_the_directory_it_created() {
        let root = TempDir::new("sudo-signal");
        let target = root.path().join("BACKUP");
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let (code, _, err) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sdb2".into(),
            },
            &runner,
            &FakeInteractive::new().push_signal(),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("terminated by a signal"), "{}", err);
        assert!(!target.exists());
    }

    #[test]
    fn a_sudo_mount_that_cannot_run_removes_the_directory() {
        let root = TempDir::new("sudo-spawn");
        let target = root.path().join("BACKUP");
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let (code, _, err) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sdb2".into(),
            },
            &runner,
            &FakeInteractive::new().push_err("sudo not found"),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("failed to run sudo"), "{}", err);
        assert!(!target.exists());
    }

    #[test]
    fn an_existing_directory_is_never_removed() {
        let root = TempDir::new("sudo-existing");
        let target = root.path().join("BACKUP");
        std::fs::create_dir_all(&target).unwrap();
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let (code, _, _) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sdb2".into(),
            },
            &runner,
            &FakeInteractive::new().push_code(1),
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(target.is_dir(), "a reused directory survives a failure");
    }

    #[test]
    fn default_mode_never_creates_the_mount_root() {
        let root = TempDir::new("udisks-no-dir");
        let config = Config {
            mount_mode: MountMode::Udisksctl,
            mount_root: root.path().join("usb"),
        };
        let runner = FakeRunner::new()
            .push_ok(TREE_JSON)
            .push_ok("Mounted /dev/sdb1 at /media/e2e/USBKEY.")
            .push_ok(TREE_JSON);
        let interactive = FakeInteractive::new();
        let (code, _, _) = run_command(
            &config,
            &Command::Mount {
                device: "sdb1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_OK);
        assert!(!config.mount_root.exists(), "no directory is created");
        assert_eq!(interactive.call_count(), 0, "no sudo");
    }

    #[test]
    fn sudo_unmount_and_force_unmount_use_umount() {
        let root = TempDir::new("sudo-umount");
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let interactive = FakeInteractive::new().push_code(0);
        let (code, out, _) = run_command(
            &sudo_config(root.path()),
            &Command::Unmount {
                device: "sdz1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("Unmounted /dev/sdz1"), "{}", out);
        assert_eq!(
            interactive.calls(),
            vec![(
                "sudo".to_string(),
                vec!["umount".to_string(), "/dev/sdz1".to_string()]
            )]
        );

        // Force unmount tries the device first and succeeds right away.
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let interactive = FakeInteractive::new().push_code(0);
        let (code, out, _) = run_command(
            &sudo_config(root.path()),
            &Command::ForceUnmount {
                device: "sdz1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("Force unmounted /dev/sdz1"), "{}", out);
        assert_eq!(
            interactive.calls(),
            vec![(
                "sudo".to_string(),
                vec![
                    "umount".to_string(),
                    "-l".to_string(),
                    "/dev/sdz1".to_string()
                ]
            )]
        );
    }

    #[test]
    fn sudo_force_unmount_falls_back_to_the_mount_point() {
        let root = TempDir::new("sudo-fallback");
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let interactive = FakeInteractive::new().push_code(1).push_code(0);
        let (code, out, err) = run_command(
            &sudo_config(root.path()),
            &Command::ForceUnmount {
                device: "sdz1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_OK, "{}", err);
        assert!(out.contains("Force unmounted /dev/sdz1"), "{}", out);
        assert_eq!(
            interactive.calls(),
            vec![
                (
                    "sudo".to_string(),
                    vec![
                        "umount".to_string(),
                        "-l".to_string(),
                        "/dev/sdz1".to_string()
                    ]
                ),
                (
                    "sudo".to_string(),
                    vec![
                        "umount".to_string(),
                        "-l".to_string(),
                        "/mnt/data".to_string()
                    ]
                ),
            ]
        );
    }

    #[test]
    fn a_failing_sudo_umount_reports_both_attempts() {
        let root = TempDir::new("sudo-fallback-fail");
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let interactive = FakeInteractive::new().push_code(1).push_code(2);
        let (code, out, err) = run_command(
            &sudo_config(root.path()),
            &Command::ForceUnmount {
                device: "sdz1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(out.is_empty(), "{}", out);
        assert!(
            err.contains("Force unmount failed for /dev/sdz1"),
            "{}",
            err
        );
        assert!(err.contains("status 2"), "{}", err);
        assert_eq!(interactive.call_count(), 2);
    }

    #[test]
    fn sudo_mode_refuses_the_same_devices_as_the_tui() {
        let root = TempDir::new("sudo-refuse");
        // A system mount is refused before sudo is asked.
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let interactive = FakeInteractive::new();
        let (code, _, err) = run_command(
            &sudo_config(root.path()),
            &Command::Unmount {
                device: "sda1".into(),
            },
            &runner,
            &interactive,
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(err.contains("system mount"), "{}", err);
        assert_eq!(interactive.call_count(), 0);

        // And nothing is created for a refused mount.
        let runner = FakeRunner::new().push_ok(TREE_JSON);
        let (_, _, err) = run_command(
            &sudo_config(root.path()),
            &Command::Mount {
                device: "sda1".into(),
            },
            &runner,
            &FakeInteractive::new(),
        );
        assert!(err.contains("already mounted"), "{}", err);
        assert!(!root.path().join("sda1").exists());
    }

    #[test]
    fn the_tui_command_does_nothing_from_run() {
        // `main` dispatches the TUI itself; `run` is a no-op for it.
        let (code, out, err) = run_command(
            &udisksctl_config(),
            &Command::Tui,
            &FakeRunner::new(),
            &FakeInteractive::new(),
        );
        assert_eq!(code, EXIT_OK);
        assert!(out.is_empty() && err.is_empty());
    }
}
