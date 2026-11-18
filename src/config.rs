//! Startup configuration: mount mode, mount root, and the override layers.
//!
//! Precedence, highest first: command line, environment, config file, defaults.
//! Everything except [`load`] is a pure function of its inputs, so the whole
//! precedence and validation policy is unit-testable without touching the real
//! process environment.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;

/// Mount root used when neither the CLI, the environment nor the config file
/// defines one; `~` expands to `$HOME`.
pub const DEFAULT_MOUNT_ROOT: &str = "~/mnt/usb";

/// Configuration directory below `$XDG_CONFIG_HOME` / `$HOME/.config`.
pub const CONFIG_DIR: &str = "mount-tui";

/// Configuration file name inside [`CONFIG_DIR`].
pub const CONFIG_FILE: &str = "config.toml";

/// Environment variable overriding the mount mode.
pub const ENV_MODE: &str = "MOUNT_TUI_MODE";

/// Environment variable overriding the mount root.
pub const ENV_MOUNT_ROOT: &str = "MOUNT_TUI_MOUNT_ROOT";

/// Mount roots that are refused outright: a removable filesystem must never be
/// mounted over the system hierarchy. Exact matches only; sub-paths such as
/// `/srv/usb` or `/mnt/data/usb` remain allowed.
const SYSTEM_ROOTS: &[&str] = &[
    "/", "/boot", "/etc", "/usr", "/var", "/home", "/root", "/opt", "/srv", "/mnt", "/media",
    "/tmp", "/run", "/proc", "/sys", "/dev",
];

/// How a mount is executed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MountMode {
    /// Default: the unprivileged `udisksctl` backend, mount point chosen by
    /// udisks2. Never escalates to sudo and never creates a directory.
    #[default]
    Udisksctl,
    /// Explicit opt-in: `sudo mount` into `mount_root/<name>`.
    Sudo,
}

impl MountMode {
    /// Parse a configured mode name (case-insensitive, surrounding blanks
    /// ignored). Returns `None` for unknown values.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "udisksctl" => Some(Self::Udisksctl),
            "sudo" => Some(Self::Sudo),
            _ => None,
        }
    }

    /// Canonical name used in messages and in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udisksctl => "udisksctl",
            Self::Sudo => "sudo",
        }
    }
}

impl fmt::Display for MountMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Effective configuration handed to the [`crate::app::App`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub mount_mode: MountMode,
    /// Absolute, `~`-expanded root for sudo mode. Never created here.
    pub mount_root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mount_mode: MountMode::Udisksctl,
            mount_root: default_mount_root(),
        }
    }
}

/// Default root, `~` expanded against `$HOME`. Infallible on purpose: without
/// `HOME` the literal is returned and [`resolve`] reports the real problem when
/// a sudo-mode configuration is actually loaded.
fn default_mount_root() -> PathBuf {
    match home_dir() {
        Some(home) => home.join("mnt").join("usb"),
        None => PathBuf::from(DEFAULT_MOUNT_ROOT),
    }
}

/// Values contributed by a single layer. Every field is optional so a layer can
/// leave a setting to the next one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub mode: Option<String>,
    pub root: Option<String>,
}

/// Resolve the effective configuration from the three layers plus `home`.
///
/// `home` is passed in instead of read from the process so tests stay hermetic;
/// `None` means `$HOME` is unavailable and `~` cannot be expanded.
pub fn resolve(
    cli: &Overrides,
    env: &Overrides,
    file: &Overrides,
    home: Option<&Path>,
) -> anyhow::Result<Config> {
    let (mode_source, mode_value) = first_value(&[
        ("command line", cli.mode.clone()),
        ("environment", env.mode.clone()),
        ("config file", file.mode.clone()),
    ]);
    let mount_mode = match mode_value {
        Some(value) => MountMode::parse(&value).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown mount mode '{}' (from {}); expected 'udisksctl' or 'sudo'",
                value,
                mode_source
            )
        })?,
        None => MountMode::default(),
    };

    let (root_source, root_value) = first_value(&[
        ("command line", cli.root.clone()),
        ("environment", env.root.clone()),
        ("config file", file.root.clone()),
    ]);
    let (root_source, raw_root, explicit_root) = match root_value {
        Some(value) => (root_source, value, true),
        None => ("default", DEFAULT_MOUNT_ROOT.to_string(), false),
    };
    // The root is only used by sudo mode; an unused default root must not stop
    // a plain `udisksctl` start just because the directory is a file/symlink.
    let check_filesystem = mount_mode == MountMode::Sudo || explicit_root;
    let mount_root = validate_root(&raw_root, home, check_filesystem)
        .with_context(|| format!("invalid mount root (from {})", root_source))?;

    Ok(Config {
        mount_mode,
        mount_root,
    })
}

/// First value found while walking the layers in precedence order, together
/// with the name of the layer that provided it.
fn first_value(layers: &[(&'static str, Option<String>)]) -> (&'static str, Option<String>) {
    for (name, value) in layers {
        if value.is_some() {
            return (name, value.clone());
        }
    }
    ("default", None)
}

/// Expand `~`, make sure the result is absolute and safe to mount under.
///
/// The value-only checks (`~` expansion, empty, relative, `~user`, system roots,
/// `$HOME` itself) always run. The filesystem checks (file / symbolic link /
/// unreadable) only run when the root is going to be used: `sudo` mode, or a
/// root that came from an explicit layer (CLI, environment, config file).
fn validate_root(
    raw: &str,
    home: Option<&Path>,
    check_filesystem: bool,
) -> anyhow::Result<PathBuf> {
    let expanded = expand_home(raw.trim(), home)?;
    if !expanded.is_absolute() {
        bail!("'{}' is not an absolute path", raw);
    }
    let normalized = normalise(&expanded);
    if SYSTEM_ROOTS
        .iter()
        .any(|root| normalized == Path::new(root))
    {
        bail!("'{}' is a system directory", normalized.display());
    }
    if let Some(home) = home {
        if normalized == normalise(home) {
            bail!("'{}' is the home directory", normalized.display());
        }
    }
    if check_filesystem {
        check_root_filesystem(&normalized)?;
    }
    Ok(normalized)
}

/// Reject roots that exist but cannot be used as a mount point.
fn check_root_filesystem(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("'{}' is a symbolic link", path.display())
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("'{}' is not a directory", path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            bail!("cannot inspect '{}': {}", path.display(), error)
        }
    }
    Ok(())
}

/// Expand a leading `~` / `~/`; `~user` is not supported (it would need the
/// passwd database and is a needless source of surprises).
fn expand_home(raw: &str, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("cannot expand '~': HOME is not set"));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home
            .map(|home| home.join(rest))
            .ok_or_else(|| anyhow::anyhow!("cannot expand '~{}': HOME is not set", rest));
    }
    if raw.starts_with('~') {
        bail!("'{}' is not supported; use '~' or '~/<path>'", raw);
    }
    if raw.is_empty() {
        bail!("the mount root must not be empty");
    }
    Ok(PathBuf::from(raw))
}

/// Drop trailing separators (`/boot/` == `/boot`); `/` stays `/`.
fn normalise(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text.len() > 1 {
        PathBuf::from(text.trim_end_matches('/'))
    } else {
        path.to_path_buf()
    }
}

/// Path of the configuration file: `$XDG_CONFIG_HOME/mount-tui/config.toml`,
/// falling back to `$HOME/.config/mount-tui/config.toml`.
pub fn config_path(home: Option<&Path>, xdg_config_home: Option<&Path>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|path| path.is_absolute()) {
        return Some(xdg.join(CONFIG_DIR).join(CONFIG_FILE));
    }
    home.map(|home| home.join(".config").join(CONFIG_DIR).join(CONFIG_FILE))
}

/// `(uid, gid)` of the invoking user's home directory; used by sudo mode when
/// naming mount targets is not enough to make a filesystem usable for a
/// non-root user. Never used to decide *where* to mount.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
}

/// Load and validate the configuration for `main`.
///
/// This is the only function that reads process state (environment, config
/// file); a broken config surfaces here before the TUI is started.
pub fn load(cli: &Overrides) -> anyhow::Result<Config> {
    let home = home_dir();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    let file = match config_path(home.as_deref(), xdg.as_deref()) {
        Some(path) => read_file(&path)?,
        None => Overrides::default(),
    };
    let env = Overrides {
        mode: env_value(ENV_MODE),
        root: env_value(ENV_MOUNT_ROOT),
    };
    resolve(cli, &env, &file, home.as_deref())
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read one config file into its layer; a missing file contributes nothing.
fn read_file(path: &Path) -> anyhow::Result<Overrides> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Overrides::default())
        }
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    parse_file(&text).with_context(|| format!("invalid config file {}", path.display()))
}

/// Raw layout of `config.toml`:
///
/// ```toml
/// [mount]
/// mode = "udisksctl" # or "sudo"
/// root = "~/mnt/usb"
/// ```
#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    mount: FileMount,
}

#[derive(Debug, Default, Deserialize)]
struct FileMount {
    mode: Option<String>,
    root: Option<String>,
}

/// Parse config file contents into their layer.
pub fn parse_file(text: &str) -> anyhow::Result<Overrides> {
    let parsed: FileConfig = toml::from_str(text).context("cannot parse TOML")?;
    let overrides = Overrides {
        mode: parsed
            .mount
            .mode
            .map(|mode| mode.trim().to_string())
            .filter(|mode| !mode.is_empty()),
        root: parsed
            .mount
            .root
            .map(|root| root.trim().to_string())
            .filter(|root| !root.is_empty()),
    };
    Ok(overrides)
}

/// Result of parsing the command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Args {
    /// Start the TUI with these command line overrides.
    Run(Overrides),
    /// `--help`: usage was printed, do not start the TUI.
    Help,
    /// `--version`: the version was printed, do not start the TUI.
    Version,
}

/// Parse the command line. `Err` carries a user-facing usage error.
pub fn parse_args<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut overrides = Overrides::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--help" | "-h" => return Ok(Args::Help),
            "--version" | "-V" => return Ok(Args::Version),
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
                } else {
                    return Err(format!("unexpected argument '{}'", arg));
                }
                index += 1;
            }
        }
    }
    Ok(Args::Run(overrides))
}

/// `--help` text. `config_path` is resolved at runtime so the user sees the
/// file this run would actually read.
pub fn help_text(config_path: Option<&Path>) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let config_path = config_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<HOME unavailable>".to_string());
    format!(
        "mount-tui {version}
Terminal disk & USB manager.

Usage:
  mount-tui [--mount-mode <udisksctl|sudo>] [--mount-root <path>]

Options:
  --mount-mode <mode>  Mount backend: 'udisksctl' (default, no sudo, mount point
                       chosen by udisks2) or 'sudo' (explicit opt-in, mounts into
                       the configured root).
  --mount-root <path>  Root directory for sudo mode (default: {default}).
                       Absolute path or '~/...'; system directories are refused.
  -h, --help           Print this help and exit.
  -V, --version        Print the version and exit.

Configuration file:
  {config_path}
  [mount]
  mode = \"udisksctl\"   # or \"sudo\"
  root = \"{default}\"

Precedence: --mount-mode/--mount-root > {ENV_MODE}/{ENV_MOUNT_ROOT} > config file
            > defaults.
",
        version = version,
        default = DEFAULT_MOUNT_ROOT,
        config_path = config_path,
    )
}

/// Default config path for the current process, used by `--help`.
pub fn current_config_path() -> Option<PathBuf> {
    let home = home_dir();
    let xdg: Option<OsString> = std::env::var_os("XDG_CONFIG_HOME");
    let xdg = xdg
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    config_path(home.as_deref(), xdg.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique scratch directory; removed by [`TempDir`] on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "mount-tui-config-{}-{}-{}",
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

    fn home() -> TempDir {
        TempDir::new("home")
    }

    fn layer(mode: Option<&str>, root: Option<&str>) -> Overrides {
        Overrides {
            mode: mode.map(str::to_string),
            root: root.map(str::to_string),
        }
    }

    /// Full error chain (context + cause), for message assertions.
    fn describe(error: anyhow::Error) -> String {
        format!("{:#}", error)
    }

    fn empty() -> Overrides {
        Overrides::default()
    }

    #[test]
    fn mode_names_are_parsed_case_insensitively() {
        assert_eq!(MountMode::parse("udisksctl"), Some(MountMode::Udisksctl));
        assert_eq!(MountMode::parse("  SUDO "), Some(MountMode::Sudo));
        assert_eq!(MountMode::parse("UDisksCtl"), Some(MountMode::Udisksctl));
        assert_eq!(MountMode::parse("polkit"), None);
        assert_eq!(MountMode::parse(""), None);
        assert_eq!(MountMode::default(), MountMode::Udisksctl);
        assert_eq!(MountMode::Sudo.as_str(), "sudo");
        assert_eq!(MountMode::Sudo.to_string(), "sudo");
    }

    #[test]
    fn defaults_are_udisksctl_and_home_mnt_usb() {
        let home = home();
        let config = resolve(&empty(), &empty(), &empty(), Some(home.path())).unwrap();
        assert_eq!(config.mount_mode, MountMode::Udisksctl);
        assert_eq!(config.mount_root, home.path().join("mnt/usb"));
    }

    #[test]
    fn defaults_expand_the_documented_literal() {
        let config = Config::default();
        // `Config::default()` must stay infallible, so without HOME the literal
        // survives; with HOME it is expanded. Either way it ends in `mnt/usb`.
        assert!(
            config.mount_root.ends_with("mnt/usb"),
            "{:?}",
            config.mount_root
        );
        assert_eq!(config.mount_mode, MountMode::Udisksctl);
    }

    #[test]
    fn config_file_sets_mode_and_root() {
        let home = home();
        let file = layer(Some("sudo"), Some("~/usb"));
        let config = resolve(&empty(), &empty(), &file, Some(home.path())).unwrap();
        assert_eq!(config.mount_mode, MountMode::Sudo);
        assert_eq!(config.mount_root, home.path().join("usb"));
    }

    #[test]
    fn environment_overrides_the_config_file() {
        let home = home();
        let file = layer(Some("sudo"), Some("~/from-file"));
        let env = layer(Some("udisksctl"), Some("~/from-env"));
        let config = resolve(&empty(), &env, &file, Some(home.path())).unwrap();
        assert_eq!(config.mount_mode, MountMode::Udisksctl);
        assert_eq!(config.mount_root, home.path().join("from-env"));
        // A layer may override only one of the two settings.
        let env = layer(Some("sudo"), None);
        let config = resolve(&empty(), &env, &file, Some(home.path())).unwrap();
        assert_eq!(config.mount_mode, MountMode::Sudo);
        assert_eq!(config.mount_root, home.path().join("from-file"));
    }

    #[test]
    fn command_line_overrides_every_other_layer() {
        let home = home();
        let cli = layer(Some("udisksctl"), Some("/var/tmp/usb"));
        let env = layer(Some("sudo"), Some("~/from-env"));
        let file = layer(Some("sudo"), Some("~/from-file"));
        let config = resolve(&cli, &env, &file, Some(home.path())).unwrap();
        assert_eq!(config.mount_mode, MountMode::Udisksctl);
        assert_eq!(config.mount_root, PathBuf::from("/var/tmp/usb"));
    }

    #[test]
    fn unknown_modes_name_the_offending_layer() {
        let home = home();
        let error = describe(
            resolve(
                &layer(Some("polkit"), None),
                &empty(),
                &empty(),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("unknown mount mode 'polkit'"), "{}", error);
        assert!(error.contains("command line"), "{}", error);
        assert!(error.contains("udisksctl"), "{}", error);
        assert!(error.contains("sudo"), "{}", error);

        let error = describe(
            resolve(
                &empty(),
                &layer(Some("root"), None),
                &empty(),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("environment"), "{}", error);

        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(Some("wheel"), None),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("config file"), "{}", error);
    }

    #[test]
    fn relative_roots_are_refused() {
        let home = home();
        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(None, Some("mnt/usb")),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("not an absolute path"), "{}", error);
        assert!(error.contains("invalid mount root"), "{}", error);
    }

    #[test]
    fn system_roots_are_refused() {
        let home = home();
        for root in [
            "/", "/boot", "/boot/", "/etc", "/usr", "/var", "/usr/", "/home", "/root", "/opt",
            "/srv", "/mnt", "/media", "/tmp", "/run", "/proc", "/sys", "/dev",
        ] {
            let error = describe(
                resolve(
                    &empty(),
                    &empty(),
                    &layer(None, Some(root)),
                    Some(home.path()),
                )
                .unwrap_err(),
            );
            assert!(
                error.contains("system directory"),
                "{} must be refused, got {}",
                root,
                error
            );
        }
        // Sub-paths below those roots and the same names below a user directory
        // are fine.
        for root in ["/srv/usb", "/mnt/data/usb", "/home/u/usb", "~/boot"] {
            assert!(
                resolve(
                    &empty(),
                    &empty(),
                    &layer(None, Some(root)),
                    Some(home.path())
                )
                .is_ok(),
                "{} must be allowed",
                root
            );
        }
    }

    #[test]
    fn the_home_directory_itself_is_refused() {
        let home = home();
        let home_text = home.path().to_string_lossy().to_string();
        let roots = [
            home_text.clone(),
            "~".to_string(),
            format!("{}/", home_text),
        ];
        for root in &roots {
            let error = describe(
                resolve(
                    &empty(),
                    &empty(),
                    &layer(None, Some(root)),
                    Some(home.path()),
                )
                .unwrap_err(),
            );
            assert!(
                error.contains("home directory"),
                "{} must be refused, got {}",
                root,
                error
            );
        }
        // Below HOME is fine.
        assert!(resolve(
            &empty(),
            &empty(),
            &layer(None, Some("~/mnt/usb")),
            Some(home.path())
        )
        .is_ok());
    }

    #[test]
    fn default_root_is_not_inspected_in_udisksctl_mode() {
        let home = home();
        std::fs::create_dir_all(home.path().join("mnt")).unwrap();

        // A symbolic link or a plain file where the default root points must not
        // stop a plain `udisksctl` start: the root is not used in that mode.
        let link = home.path().join("mnt/usb");
        let real = home.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();
        let config = resolve(&empty(), &empty(), &empty(), Some(home.path())).unwrap();
        assert_eq!(config.mount_mode, MountMode::Udisksctl);
        assert_eq!(config.mount_root, link);

        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, b"x").unwrap();
        let config = resolve(&empty(), &empty(), &empty(), Some(home.path())).unwrap();
        assert_eq!(config.mount_root, link);

        // sudo mode actually uses the root, so the default root is inspected.
        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(Some("sudo"), None),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("not a directory"), "{}", error);
    }

    #[test]
    fn explicit_root_is_inspected_in_udisksctl_mode() {
        let home = home();
        let file_path = home.path().join("plain-file");
        std::fs::write(&file_path, b"x").unwrap();
        let real = home.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = home.path().join("link");
        symlink(&real, &link).unwrap();

        // Every explicit layer is inspected even when the mode is udisksctl.
        let cli = layer(Some("udisksctl"), Some(&file_path.to_string_lossy()));
        let error = describe(resolve(&cli, &empty(), &empty(), Some(home.path())).unwrap_err());
        assert!(error.contains("not a directory"), "{}", error);
        assert!(error.contains("command line"), "{}", error);

        let env = layer(Some("udisksctl"), Some(&link.to_string_lossy()));
        let error = describe(resolve(&empty(), &env, &empty(), Some(home.path())).unwrap_err());
        assert!(error.contains("symbolic link"), "{}", error);
        assert!(error.contains("environment"), "{}", error);

        let file = layer(None, Some(&file_path.to_string_lossy()));
        let error = describe(resolve(&empty(), &empty(), &file, Some(home.path())).unwrap_err());
        assert!(error.contains("not a directory"), "{}", error);
        assert!(error.contains("config file"), "{}", error);
    }

    #[test]
    fn value_only_checks_apply_in_every_mode() {
        let home = home();
        for mode in ["udisksctl", "sudo"] {
            let error = describe(
                resolve(
                    &empty(),
                    &empty(),
                    &layer(Some(mode), Some("mnt/usb")),
                    Some(home.path()),
                )
                .unwrap_err(),
            );
            assert!(error.contains("not an absolute path"), "{}", error);

            let error = describe(
                resolve(
                    &empty(),
                    &empty(),
                    &layer(Some(mode), Some("/mnt")),
                    Some(home.path()),
                )
                .unwrap_err(),
            );
            assert!(error.contains("system directory"), "{}", error);

            let error = describe(
                resolve(
                    &empty(),
                    &empty(),
                    &layer(Some(mode), Some("~other/usb")),
                    Some(home.path()),
                )
                .unwrap_err(),
            );
            assert!(error.contains("not supported"), "{}", error);
        }
    }

    #[test]
    fn trailing_separators_are_normalised() {
        let home = home();
        let config = resolve(
            &empty(),
            &empty(),
            &layer(None, Some("~/mnt/usb/")),
            Some(home.path()),
        )
        .unwrap();
        assert_eq!(config.mount_root, home.path().join("mnt/usb"));

        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(None, Some("/boot//")),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("system directory"), "{}", error);
    }

    #[test]
    fn tilde_forms_are_expanded_and_bad_ones_refused() {
        let home = home();
        // `~` expands to HOME, which is itself refused as a mount root.
        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(None, Some("~")),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("home directory"), "{}", error);

        let config = resolve(
            &empty(),
            &empty(),
            &layer(None, Some("~/a/b")),
            Some(home.path()),
        )
        .unwrap();
        assert_eq!(config.mount_root, home.path().join("a/b"));

        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(None, Some("~other/usb")),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("not supported"), "{}", error);

        // `~` without HOME cannot be expanded.
        let error =
            describe(resolve(&empty(), &empty(), &layer(None, Some("~/usb")), None).unwrap_err());
        assert!(error.contains("HOME is not set"), "{}", error);
    }

    #[test]
    fn a_file_or_symlink_root_is_refused() {
        let home = home();
        let file_path = home.path().join("plain-file");
        std::fs::write(&file_path, b"x").unwrap();
        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(None, Some(&file_path.to_string_lossy())),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("not a directory"), "{}", error);

        let real = home.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = home.path().join("link");
        symlink(&real, &link).unwrap();
        let error = describe(
            resolve(
                &empty(),
                &empty(),
                &layer(None, Some(&link.to_string_lossy())),
                Some(home.path()),
            )
            .unwrap_err(),
        );
        assert!(error.contains("symbolic link"), "{}", error);
    }

    #[test]
    fn an_existing_directory_root_is_accepted() {
        let home = home();
        let root = home.path().join("usb");
        std::fs::create_dir_all(&root).unwrap();
        let config = resolve(
            &empty(),
            &empty(),
            &layer(None, Some(&root.to_string_lossy())),
            Some(home.path()),
        )
        .unwrap();
        assert_eq!(config.mount_root, root);
    }

    #[test]
    fn config_file_parsing_reads_the_mount_section() {
        let parsed = parse_file("[mount]\nmode = \"sudo\"\nroot = \"~/usb\"\n").unwrap();
        assert_eq!(parsed, layer(Some("sudo"), Some("~/usb")));

        // Missing file contents / missing section / blanks keep the defaults.
        assert_eq!(parse_file("").unwrap(), empty());
        assert_eq!(parse_file("# only a comment\n").unwrap(), empty());
        assert_eq!(parse_file("[mount]\n").unwrap(), empty());
        assert_eq!(
            parse_file("[mount]\nmode = \"  \"\nroot = \"\"\n").unwrap(),
            empty()
        );

        // Unknown keys are ignored, an invalid mode is only rejected later.
        let parsed = parse_file("[mount]\nmode = \"polkit\"\nextra = 1\n").unwrap();
        assert_eq!(parsed, layer(Some("polkit"), None));
    }

    #[test]
    fn config_file_parsing_reports_broken_toml() {
        let error = parse_file("[mount\nmode = 1").unwrap_err().to_string();
        assert!(error.contains("cannot parse TOML"), "{}", error);
    }

    #[test]
    fn config_path_prefers_xdg_config_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            config_path(Some(home), Some(Path::new("/xdg"))),
            Some(PathBuf::from("/xdg/mount-tui/config.toml"))
        );
        // A relative XDG_CONFIG_HOME is ignored (per the XDG spec).
        assert_eq!(
            config_path(Some(home), Some(Path::new("relative"))),
            Some(PathBuf::from("/home/u/.config/mount-tui/config.toml"))
        );
        assert_eq!(
            config_path(Some(home), None),
            Some(PathBuf::from("/home/u/.config/mount-tui/config.toml"))
        );
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn command_line_parsing_accepts_both_value_forms() {
        assert_eq!(
            parse_args(["--mount-mode", "sudo", "--mount-root", "/srv/usb"]).unwrap(),
            Args::Run(layer(Some("sudo"), Some("/srv/usb")))
        );
        assert_eq!(
            parse_args(["--mount-mode=sudo", "--mount-root=~/usb"]).unwrap(),
            Args::Run(layer(Some("sudo"), Some("~/usb")))
        );
        assert_eq!(
            parse_args(std::iter::empty::<String>()).unwrap(),
            Args::Run(empty())
        );
        assert_eq!(parse_args(["--help"]).unwrap(), Args::Help);
        assert_eq!(parse_args(["-h"]).unwrap(), Args::Help);
        assert_eq!(parse_args(["--version"]).unwrap(), Args::Version);
        assert_eq!(parse_args(["-V"]).unwrap(), Args::Version);
        // Later values win, exactly like any other CLI parser.
        assert_eq!(
            parse_args(["--mount-mode", "sudo", "--mount-mode", "udisksctl"]).unwrap(),
            Args::Run(layer(Some("udisksctl"), None))
        );
    }

    #[test]
    fn command_line_parsing_rejects_useless_input() {
        let error = parse_args(["--mount-mode"]).unwrap_err();
        assert!(error.contains("needs a value"), "{}", error);
        let error = parse_args(["--mount-root"]).unwrap_err();
        assert!(error.contains("needs a value"), "{}", error);
        let error = parse_args(["--nope"]).unwrap_err();
        assert!(error.contains("unknown option"), "{}", error);
        let error = parse_args(["extra"]).unwrap_err();
        assert!(error.contains("unexpected argument"), "{}", error);
    }

    #[test]
    fn help_text_documents_the_layers_and_defaults() {
        let text = help_text(Some(Path::new("/home/u/.config/mount-tui/config.toml")));
        assert!(text.contains("--mount-mode"), "{}", text);
        assert!(text.contains("--mount-root"), "{}", text);
        assert!(text.contains(DEFAULT_MOUNT_ROOT), "{}", text);
        assert!(
            text.contains("/home/u/.config/mount-tui/config.toml"),
            "{}",
            text
        );
        assert!(text.contains(ENV_MODE), "{}", text);
        assert!(text.contains(ENV_MOUNT_ROOT), "{}", text);
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{}", text);
        assert!(help_text(None).contains("<HOME unavailable>"));
    }
}
