# Changelog

All notable user-facing changes are documented here.

## [0.1.0] - 2026-11-18

### Added

- Terminal UI with a device tree, details pane, status bar and keyboard help.
- Mount, unmount and forced unmount; forced unmount requires a second confirmation.
- Protection for system mounts and devices with multiple mount points.
- Two mount modes: unprivileged `udisksctl` (default) and opt-in `sudo` with a
  configurable mount root.
- Label-based sudo mount targets with collision handling and filesystem-aware
  ownership options.
- Automatic device refresh, labels, read-only/removable tags, multi-mount
  display, search, loop-device filtering and scrollable details.
- Non-interactive CLI: `list`, `status`, `mount`, `unmount`,
  `force-unmount --yes`, JSON output and `0/1/2` exit codes.
- Dual `MIT OR Apache-2.0` licensing.
