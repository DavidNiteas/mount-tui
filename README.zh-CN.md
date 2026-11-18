# Mount-TUI

[English](README.md) | **简体中文**

终端里的磁盘与 USB 管理器：查看块设备树，挂载 / 卸载可移动介质，并提供脚本化的
非交互 CLI。

TUI 左侧是设备树（含分区、LVM、crypt 子节点），右侧是选中设备的详情（型号、
传输方式、文件系统、卷标、UUID、挂载点、是否受保护），底部是状态栏与键位提示。

## 功能

- 设备树、分区 / LVM / crypt 子节点、USB / 可移动 / 只读 / 卷标标记；
- 挂载、卸载、强制卸载（二次确认）；
- 系统盘保护：`/`、`/boot`、`/home`、`/home/<user>` 及系统前缀不会被卸载；
- 多挂载点保护：同一设备多个挂载点时拒绝自动卸载；
- 自动刷新：插拔设备后自动更新列表，保留选中与折叠状态；
- 搜索、隐藏 loop 设备、详情滚动、小终端自适应布局；
- 两种挂载后端：默认非 root 的 `udisksctl`，或显式启用的 `sudo` + 自定义挂载根；
- 非交互 CLI：`list`、`status`、`mount`、`unmount`、`force-unmount`，支持 JSON。

## 依赖

- Linux；
- `lsblk`（util-linux）；
- 默认模式需要 `udisksctl`（udisks2）；`sudo` 模式需要可用的 `sudo`；
- `fuser` / `lsof` 可选：卸载时用于列出占用进程。

## 安装

```bash
cargo install --path .
```

也可以直接构建：

```bash
cargo build --release
./target/release/mount-tui
```

## 运行

```bash
mount-tui
```

默认进入 TUI。命令行用法见下方「CLI」。

## 两种挂载模式

| 模式 | 是否需要 root | 挂载点 | 默认 |
|---|---|---|---|
| `udisksctl` | 否 | 由 udisks2 决定（通常 `/media/<user>/<label>`） | 是 |
| `sudo` | 是 | 可配置挂载根下（默认 `~/mnt/usb`） | 否，显式开启 |

默认模式不会调用 sudo，也不会创建挂载点目录。需要把设备固定挂到自己的目录时，
显式启用 sudo 模式。

## 配置

配置文件：

```text
$XDG_CONFIG_HOME/mount-tui/config.toml
# 未设置 XDG_CONFIG_HOME 时：
$HOME/.config/mount-tui/config.toml
```

```toml
[mount]
mode = "udisksctl"   # 或 "sudo"
root = "~/mnt/usb"   # 仅 sudo 模式使用
```

优先级：`--mount-mode` / `--mount-root` > 环境变量 `MOUNT_TUI_MODE` /
`MOUNT_TUI_MOUNT_ROOT` > 配置文件 > 默认值。

`root` 支持 `~` / `~/...` 与绝对路径；系统目录（`/`、`/boot`、`/home`、`/mnt`、
`/media`、`/tmp` 等）和 `$HOME` 本身会被拒绝。`sudo` 模式的挂载点优先使用卷标，
无卷标时使用设备名；冲突自动加后缀，符号链接目标会被拒绝。FAT/exFAT/NTFS 会自动
补充 `uid`/`gid`/`umask`，保证普通用户可写。

## 键位

| 按键 | 作用 |
|---|---|
| `Up` / `Down` | 上下移动选择 |
| `PgUp` / `PgDn` | 翻页 |
| `Home` / `End` | 跳到首 / 尾 |
| `Left` / `Right` | 折叠 / 展开当前节点 |
| `Enter` | 未挂载则挂载，已挂载则卸载 |
| `m` | 挂载选中设备 |
| `u` | 卸载选中设备 |
| `f` | 强制卸载：第一次提示，第二次确认后执行 |
| `r` | 立即刷新设备列表 |
| `/` | 搜索（name / label / model / fstype / 挂载点） |
| `l` | 显示 / 隐藏 loop 设备 |
| `[` / `]` | 详情上 / 下滚动 |
| `q` / `Esc` / `Ctrl-C` | 退出（有搜索过滤时 `Esc` 先清过滤） |

搜索输入态下字符直接进入查询，`Enter` 保留过滤、`Esc` 取消；`q` 是普通字符。

## 挂载与卸载

| 动作 | `udisksctl` 模式 | `sudo` 模式 |
|---|---|---|
| 挂载 | `udisksctl mount -b /dev/X` | `sudo mount -o <options> /dev/X <root>/<label-or-name>` |
| 卸载 | `udisksctl unmount -b /dev/X` | `sudo umount /dev/X` |
| 强制卸载 | `udisksctl unmount -b /dev/X -f` | `sudo umount -l /dev/X` |

sudo 模式的挂载点只在执行前创建；挂载失败或取消时，本次新建的空目录会被清理。
`udisksctl` 模式失败时只报告错误，不会自动改用 sudo。

## 强制卸载

`f` 是破坏性操作：第一次按下只显示确认提示，同一设备再按一次才执行；移动选择、
翻页、折叠、`m` / `u` / `Enter` 或 `r` 刷新都会取消确认。CLI 的
`force-unmount` 需要显式 `--yes`。

## 系统盘保护

卸载前会检查：

1. 设备必须已挂载；
2. 设备的任意挂载点属于系统路径即拒绝：`/`、`/boot`、`/home`、`/home/<user>`、
   `/usr`、`/var`、`/etc`、`/opt`、`/root`、`/srv`、`/tmp` 及对应前缀；
3. 多挂载点设备只允许手动处理。

## CLI（非交互）

```text
mount-tui list [--json]
mount-tui status <dev> [--json]
mount-tui mount <dev>
mount-tui unmount <dev>
mount-tui force-unmount <dev> --yes
```

`<dev>` 支持设备名（`sdb1`）、路径（`/dev/sdb1`）或唯一卷标（大小写不敏感）。

示例：

```bash
mount-tui list
mount-tui list --json
mount-tui status USBKEY --json
mount-tui mount sdb1
mount-tui unmount sdb1
mount-tui force-unmount sdb1 --yes
```

退出码：`0` 成功，`1` 操作失败，`2` 用法或配置错误。`--json` 只用于
`list` / `status`；JSON 字段只追加，不改名或删除。

## 许可

本项目采用 Rust 同款双许可，任选其一：

- Apache License, Version 2.0（[`LICENSE-APACHE`](LICENSE-APACHE)）
- MIT License（[`LICENSE-MIT`](LICENSE-MIT)）

除非你明确声明，否则任何有意提交以纳入本项目的贡献，均按上述双许可授权，不附加
其他条款或条件。

## 使用限制

- `udisksctl` 模式无法指定挂载点，挂载位置由 udisks2 决定；需要固定位置请使用
  sudo 模式。
- sudo 模式需要终端认证，不适合无人值守场景。
- 不处理 LUKS 解锁、`fstab` 编辑或额外挂载选项。
- 状态栏最多 8 行；超长消息可能被裁剪。
- 终端高度不足 14 行时键位提示自动隐藏。
- 命令执行期间界面可能短暂无响应；CLI 每次调用都会重新读取设备。
