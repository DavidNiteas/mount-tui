# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-18

### Added

- Terminal UI with a device tree, details pane, status bar and keyboard help.
- Mount, unmount and lazy/forced unmount with a two-step force confirmation.
- System-mount protection and refusal to auto-unmount devices with multiple
  mount points.
- Two mount backends: unprivileged `udisksctl` (default, mount point chosen by
  udisks2) and an opt-in `sudo` mode with a configurable mount root.
- `~/.config/mount-tui/config.toml` plus environment and CLI overrides for
  mount mode/root.
- Label-first sudo mount targets with collision handling, symlink refusal and
  filesystem-aware `uid`/`gid`/`umask` options.
- Automatic device refresh, LABEL/RO/RM tags, multi-mount display, search,
  loop-device filter, details scrolling and small-terminal layout.
- Non-interactive CLI: `list`, `status`, `mount`, `unmount`,
  `force-unmount --yes`, `--json` output and `0/1/2` exit codes.
- Injected command-runner test layer and mocked end-to-end scenarios.

### Safety

- Refuses to unmount `/`, `/boot`, `/home`, `/home/<user>`, system prefixes and
  other protected roots.
- Default mode never escalates to `sudo` and never creates mount-point
  directories.
- Sudo mounts create their target immediately before execution and remove it
  again when the mount fails or is cancelled.
- Force unmount requires a second confirmation in the TUI or `--yes` in the CLI.

### Documentation

- README with TUI/CLI usage, modes, safety rules, tests and limitations.
- Dual `MIT OR Apache-2.0` licensing and contributing guide.
