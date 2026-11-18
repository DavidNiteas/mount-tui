# Mount-TUI

**English** | [简体中文](README.zh-CN.md)

A terminal disk and USB manager for Linux: browse block devices, mount and unmount
removable media, and script the same operations through a non-interactive CLI.

The TUI shows a device tree on the left (including partitions, LVM and crypt
children) and details for the selected device on the right (model, transport,
filesystem, label, UUID, mount points and protection status). The status bar and
key hints sit at the bottom.

## Features

- Device tree with partitions, LVM/crypt children, USB/removable/read-only/label
  tags;
- Mount, unmount and forced unmount (with confirmation);
- System-disk protection: `/`, `/boot`, `/home`, `/home/<user>` and system
  prefixes are never unmounted;
- Multi-mount protection: a device with several mount points is refused;
- Automatic refresh: hotplugged devices appear without a keypress, preserving
  selection and collapse state;
- Search, loop-device filtering, scrollable details and a compact layout for
  short terminals;
- Two mount backends: unprivileged `udisksctl` by default, or opt-in `sudo` with
  a configurable mount root;
- Non-interactive CLI: `list`, `status`, `mount`, `unmount`, `force-unmount`,
  with JSON output.

## Requirements

- Linux;
- `lsblk` from util-linux;
- `udisksctl` (udisks2) for the default mode; `sudo` for the opt-in sudo mode;
- Optional `fuser` / `lsof`: used to list processes when an unmount is busy.

## Installation

```bash
cargo install --path .
```

Or build directly:

```bash
cargo build --release
./target/release/mount-tui
```

## Usage

```bash
mount-tui
```

The default command opens the TUI. See [CLI](#cli) for non-interactive commands.

## Mount modes

| Mode | Root required | Mount point | Default |
|---|---|---|---|
| `udisksctl` | No | Chosen by udisks2 (usually `/media/<user>/<label>`) | Yes |
| `sudo` | Yes | Under a configurable mount root (default `~/mnt/usb`) | No, opt-in |

The default mode never calls sudo and never creates mount-point directories.
Enable sudo mode when you need devices mounted at a fixed location.

## Configuration

Configuration file:

```text
$XDG_CONFIG_HOME/mount-tui/config.toml
# or, when XDG_CONFIG_HOME is unset:
$HOME/.config/mount-tui/config.toml
```

```toml
[mount]
mode = "udisksctl"   # or "sudo"
root = "~/mnt/usb"   # sudo mode only
```

Precedence: `--mount-mode` / `--mount-root` > environment variables
`MOUNT_TUI_MODE` / `MOUNT_TUI_MOUNT_ROOT` > config file > defaults.

`root` supports `~`, `~/...` and absolute paths. System directories (`/`,
`/boot`, `/home`, `/mnt`, `/media`, `/tmp`, etc.) and `$HOME` itself are refused.
In sudo mode, mount points use the filesystem label first, then the device name;
collisions get a suffix and symbolic-link targets are refused. FAT/exFAT/NTFS
filesystems automatically receive `uid`/`gid`/`umask` so they remain writable for
the invoking user.

## Key bindings

| Key | Action |
|---|---|
| `Up` / `Down` | Move selection |
| `PgUp` / `PgDn` | Page up / down |
| `Home` / `End` | Jump to first / last device |
| `Left` / `Right` | Collapse / expand the selected node |
| `Enter` | Mount if unmounted, unmount if mounted |
| `m` | Mount the selected device |
| `u` | Unmount the selected device |
| `f` | Force unmount: first press arms, second press confirms |
| `r` | Refresh the device list now |
| `/` | Search by name, label, model, fstype or mount point |
| `l` | Show / hide loop devices |
| `[` / `]` | Scroll details up / down |
| `q` / `Esc` / `Ctrl-C` | Quit (`Esc` clears an active filter first) |

In the search input, characters go into the query; `Enter` keeps the filter and
`Esc` cancels it. `q` is a normal character while searching.

## Mounting and unmounting

| Action | `udisksctl` mode | `sudo` mode |
|---|---|---|
| Mount | `udisksctl mount -b /dev/X` | `sudo mount -o <options> /dev/X <root>/<label-or-name>` |
| Unmount | `udisksctl unmount -b /dev/X` | `sudo umount /dev/X` |
| Force unmount | `udisksctl unmount -b /dev/X -f` | `sudo umount -l /dev/X` |

In sudo mode the target directory is created only immediately before the mount;
if the mount fails or is cancelled, the directory created by that attempt is
removed when empty. In udisksctl mode failures are reported without silently
switching to sudo.

## Forced unmount

`f` is destructive: the first press only shows a confirmation, and a second press
on the same device executes it. Moving the selection, paging, collapsing,
`m` / `u` / `Enter` or `r` cancels the confirmation. The CLI
`force-unmount` requires an explicit `--yes`.

## System mount protection

Before unmounting:

1. The device must be mounted.
2. Any mount point under a protected path refuses the operation: `/`, `/boot`,
   `/home`, `/home/<user>`, `/usr`, `/var`, `/etc`, `/opt`, `/root`, `/srv`,
   `/tmp` and their system prefixes.
3. A device with multiple mount points must be handled manually.

## CLI

```text
mount-tui list [--json]
mount-tui status <dev> [--json]
mount-tui mount <dev>
mount-tui unmount <dev>
mount-tui force-unmount <dev> --yes
```

`<dev>` accepts a device name (`sdb1`), a path (`/dev/sdb1`) or a unique label
(case-insensitive).

Examples:

```bash
mount-tui list
mount-tui list --json
mount-tui status USBKEY --json
mount-tui mount sdb1
mount-tui unmount sdb1
mount-tui force-unmount sdb1 --yes
```

Exit codes: `0` success, `1` operation failure, `2` usage or configuration
error. `--json` is available for `list` and `status`; JSON fields are append-only
and are never renamed or removed.

## License

Licensed under either of:

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

## Limitations

- In udisksctl mode the mount point cannot be chosen; it is decided by udisks2.
  Use sudo mode for a fixed location.
- Sudo mode requires interactive authentication and is not suitable for
  unattended operation.
- LUKS unlocking, `fstab` editing and extra mount options are not handled.
- The status bar is capped at 8 rows; very long messages may be truncated.
- Key hints are hidden when the terminal is shorter than 14 rows.
- The UI may pause briefly while a command runs; the CLI re-reads devices on
  every invocation.
