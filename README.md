# Mount-TUI

终端里的磁盘与 USB 管理器：列出块设备树，挂载 / 卸载可移动介质，并对系统盘做
保护。界面左右两栏——左侧设备树（含分区、LVM、crypt 子节点），右侧选中设备的
详情（型号、传输方式、文件系统、卷标、UUID、挂载点、是否受保护），底部是状态栏
与键位提示。

同一个二进制也提供非交互子命令（`list` / `status` / `mount` / `unmount` /
`force-unmount`，见「命令行（非交互）」），供脚本使用。

状态栏按实际渲染行数自动增高（上限 8 行）：8 行以内的 CJK 长错误不会被截断，
超过上限仍会裁剪。设备多时可以用搜索过滤列表，`l` 隐藏 loop 设备（snap 镜像等），
右侧详情可用 `[` / `]` 滚动；终端太矮时键位提示自动收起，把行数让给设备列表。

## 两种挂载模式

| 模式 | 是否需要 root | 挂载点 | 副作用 |
|---|---|---|---|
| `udisksctl`（默认） | 否 | 由 udisks2 决定（通常 `/media/<user>/<label>`） | 不创建任何目录 |
| `sudo`（显式开启） | 是 | 可配置的挂载根下，默认 `~/mnt/usb` | 目标目录只在执行前创建，失败/取消后清理 |

默认模式不会提权、不会回退 `sudo`、不会创建目录；需要自定义挂载位置的人显式
启用 sudo 模式。

挂载根只在真正会被用到时才检查文件系统（是否为文件 / 符号链接 / 不可读）：
`sudo` 模式，或 `root` 来自显式配置（命令行 / 环境变量 / 配置文件）。因此默认
`udisksctl` 模式下，即使 `~/mnt/usb` 不存在、是文件或符号链接，也不会阻止启动；
纯值校验（`~` 展开、相对路径、系统目录、`$HOME` 本身）在所有模式下照常执行。

## 依赖

- Linux（读取 `/proc`/`lsblk` 的块设备信息）
- `lsblk`（util-linux）；旧版本缺少 `MOUNTPOINTS` 列时会自动回退到旧列
- `udisksctl`（udisks2）：默认模式使用
- `sudo`：仅 sudo 模式使用
- `fuser` / `lsof`（可选）：默认模式卸载遇 `busy` 时用于列出占用进程
- `tmux`：仅端到端测试需要

## 构建

```bash
cargo build
```

产物：`target/debug/mount-tui`。Rust 依赖只有 ratatui / crossterm / serde /
serde_json / anyhow / toml；系统命令依赖见上一节。本仓库开发环境依赖已缓存，
使用 `--offline` 也可构建。

## 运行

```bash
./target/debug/mount-tui                       # 默认 udisksctl 模式
./target/debug/mount-tui --mount-mode sudo     # 显式 sudo 模式，默认根 ~/mnt/usb
./target/debug/mount-tui --help
./target/debug/mount-tui --version
```

启动时先读取配置与设备列表，读取失败会在 TUI 状态栏里显示原因，而不是直接退出；
**配置错误**（未知模式、非法挂载根等）在进入 TUI 之前打印到 stderr 并以非零退出码
结束。运行期出现无法恢复的错误（例如终端初始化失败）时，程序先恢复终端，再以
**非零退出码**结束——脚本可以据此判断成败。

## 命令行（非交互）

同一个二进制也能在脚本里使用：不初始化终端、不进入 raw mode / 备用屏，直接读写
stdout / stderr 并返回退出码。

```bash
mount-tui                                  # 默认：启动 TUI
mount-tui tui                              # 显式启动 TUI
mount-tui list                             # 设备树（人类可读）
mount-tui list --json                      # 设备树（JSON）
mount-tui status sdb1                      # 单个设备的字段与挂载状态
mount-tui status /dev/sdb1 --json
mount-tui mount sdb1
mount-tui unmount sdb1
mount-tui force-unmount sdb1 --yes
mount-tui --help | --version
```

`--mount-mode` / `--mount-root` 对所有命令生效，位置可以在子命令前后；配置文件与
环境变量的优先级与 TUI 完全相同（见下一节）。

### 设备参数

`<dev>` 可以是：

- 设备名：`sdb1`；
- 设备路径：`/dev/sdb1`；
- 卷标：`USBKEY`（大小写不敏感，首尾空白忽略）。

查找会遍历整棵设备树（含分区、crypt、LVM 子节点）。**找不到**或**匹配到多个**
（例如两个分区同名卷标）时按用法错误处理（退出码 2），stderr 会列出可用名称或
歧义项。

### list / status

`list` 每行一个设备，子节点缩进两格：

```text
sda 100G disk - - -
  sda1 100G part ext4 - /home [sys]
sdb 8G disk - - - [usb] [rm]
  sdb1 8G part vfat USBKEY - [usb] [rm]
sdy1 1T part ext4 MULTI /mnt/a,/mnt/b
```

列依次为：名称、容量、类型、文件系统、卷标、挂载点（多个用 `,` 连接，未挂载为
`-`），随后是标记：`[sys]`（挂载在受保护的系统路径上）、`[usb]`、`[ro]`
（只读）、`[rm]`（可移动）。

`status` 输出同样的字段加派生状态（`mounted` / `mountable` / `protected` /
`multi_mount`）：

```text
path: /dev/sdb1
name: sdb1
size: 8G
type: part
fstype: vfat
label: USBKEY
uuid: ABCD-1234
...
mountpoints: -
mounted: false
mountable: true
protected: false
multi_mount: false
```

`--json` 输出到 stdout（错误仍走 stderr），可以直接交给 `jq`：

```console
$ mount-tui status USBKEY --json
{
  "path": "/dev/sdb1",
  "name": "sdb1",
  "size": "8G",
  "type": "part",
  "fstype": "vfat",
  "label": "USBKEY",
  "uuid": "ABCD-1234",
  "model": null,
  "vendor": null,
  "tran": "usb",
  "serial": null,
  "mountpoints": [],
  "ro": false,
  "rm": true,
  "usb": true,
  "mounted": false,
  "mountable": true,
  "protected": false,
  "multi_mount": false
}
```

`list --json` 是设备对象数组，每个对象带 `children` 数组（子节点同构，叶子为空
数组）：

```console
$ mount-tui list --json | jq -r '.[] | [.name, (.children|length)] | @tsv'
```

### mount / unmount / force-unmount

| 命令 | `udisksctl`（默认） | `sudo`（opt-in） |
|---|---|---|
| `mount <dev>` | `udisksctl mount -b /dev/X` | `sudo mount -o <options> /dev/X <root>/<label-or-name>` |
| `unmount <dev>` | `udisksctl unmount -b /dev/X` | `sudo umount /dev/X` |
| `force-unmount <dev> --yes` | `udisksctl unmount -b /dev/X -f` | `sudo umount -l /dev/X`（失败再对挂载点重试） |

- **默认模式不提权、不创建目录**：`udisksctl` 失败就是失败（退出码 1），不会回退
  `sudo`。
- **sudo 模式**复用 TUI 的挂载点规则：目标在配置的根目录下、冲突时加后缀、符号
  链接拒绝；执行前创建目标目录（stdin/stdout/stderr 继承，`sudo` 可正常提示密码），
  本次新建的目标在失败或取消后删除（仅当仍为空）。
- 成功后重新读取设备并打印**真实**挂载点（读不到时退回计划路径 / 仅报完成）；
  失败时把 `udisksctl` / `sudo` 的错误打印到 stderr。
- 安全规则与 TUI 完全一致：挂载要求有文件系统且未挂载；卸载/强制卸载要求已挂载、
  不是系统盘（任一挂载点）、不是多挂载点。拒绝时不执行任何命令，退出码 1。
- `force-unmount` 必须带 `--yes`（强制卸载是惰性摘除挂载点，仍写盘的进程可能丢
  数据），否则按用法错误退出 2。

### 退出码

| 码 | 含义 |
|---|---|
| `0` | 成功 |
| `1` | 操作失败（命令失败、`lsblk` 读不到、安全拒绝） |
| `2` | 用法或配置错误（未知子命令/参数、缺 `--yes`、设备不存在或有歧义、配置非法） |

脚本示例：

```bash
if mount-tui mount USBKEY >/dev/null 2>&1; then
    echo "mounted at $(mount-tui status USBKEY --json | jq -r '.mountpoints[0]')"
else
    echo "mount failed" >&2
fi
```

## 配置

配置文件：`$XDG_CONFIG_HOME/mount-tui/config.toml`，未设置 `$XDG_CONFIG_HOME`
时为 `$HOME/.config/mount-tui/config.toml`。

```toml
[mount]
mode = "udisksctl"   # 或 "sudo"
root = "~/mnt/usb"   # 仅 sudo 模式使用
```

优先级（高到低）：命令行 `--mount-mode` / `--mount-root` > 环境变量
`MOUNT_TUI_MODE` / `MOUNT_TUI_MOUNT_ROOT` > 配置文件 > 默认值
（`udisksctl` + `~/mnt/usb`）。

`root` 支持 `~`、`~/...` 与绝对路径；某一项为空或未写时用默认值。以下配置会被拒绝
（stderr + 非零退出）：

- `mode` 不是 `udisksctl` / `sudo`；
- `root` 是相对路径，或使用了 `~user` 形式；
- `root` 精确等于系统目录：`/`、`/boot`、`/etc`、`/usr`、`/var`、`/home`、`/root`、
  `/opt`、`/srv`、`/mnt`、`/media`、`/tmp`、`/run`、`/proc`、`/sys`、`/dev`
  （末尾 `/` 同样拒绝；子路径如 `/srv/usb`、`/mnt/data/usb` 允许）；
- `root` 等于当前 `$HOME` 本身；
- `root` 指向一个文件或符号链接（仅 `sudo` 模式或显式配置的 `root` 会检查）。

自定义挂载根的例子：

```toml
[mount]
mode = "sudo"
root = "/srv/usb"
```

临时覆盖（不写配置文件）：

```bash
MOUNT_TUI_MODE=sudo MOUNT_TUI_MOUNT_ROOT=/srv/usb ./target/debug/mount-tui
./target/debug/mount-tui --mount-mode sudo --mount-root /srv/usb
```

## 键位

| 按键 | 作用 |
|---|---|
| `Up` / `Down` | 上下移动选择 |
| `PgUp` / `PgDn` | 翻页（10 行） |
| `Home` / `End` | 跳到首 / 尾 |
| `Left` / `Right` | 折叠 / 展开当前节点 |
| `Enter` | 未挂载则挂载，已挂载则卸载；不可挂载时说明原因 |
| `m` | 挂载选中设备 |
| `u` | 卸载选中设备 |
| `f` | 强制卸载：第一次按下只提示确认，同一设备再按一次才执行 |
| `r` | 立即刷新设备列表 |
| `/` | 搜索：输入即过滤，`Enter` 保留查询、`Esc` 取消 |
| `l` | 显示 / 隐藏 loop 设备 |
| `[` / `]` | 详情面板向上 / 向下滚动 |
| `q` / `Ctrl-C` | 退出 |
| `Esc` | 有查询时先清除查询；没有查询时退出 |

## 列表视图：搜索、loop 与详情滚动

**搜索**：`/` 打开搜索行（旧查询会被清空），输入即过滤列表。

- 匹配字段（大小写不敏感，任一命中即可）：设备名、卷标、型号、文件系统类型、
  挂载点；
- 设备本身命中时保留它**及其全部子节点**；只有子节点命中时保留父节点与命中分支；
- 过滤期间**忽略折叠状态**，命中项一定可见；清除查询后恢复原来的折叠状态；
- 没有命中时列表为空，右侧显示 `No device selected`；
- `Enter` 保留查询并回到列表（键位提示行显示 `Filter: <query> (/ edit, Esc clear)`），
  `Esc` 在输入态取消并清空；列表态 `Esc` 先清除已应用的查询，再按一次才退出；
- 输入态下所有可打印键都是查询字符（包括 `q` / `m` / `u`），只有 `Enter`、`Esc`、
  `Backspace` 与 `Ctrl-C`（退出）有特殊含义。

**loop 设备**：`type=loop` 的设备（snap 镜像、挂载的 ISO 等）默认隐藏，`l` 切换。
切换后状态栏显示 `Loop devices shown` / `Loop devices hidden`；命中搜索的 loop
设备同样受该过滤影响。

**详情滚动**：`]` / `[` 向下 / 向上滚动右侧详情，最多 100 行；切换选中项、搜索或
loop 过滤变化、刷新导致选中项改变时都回到顶部。长字段按宽度换行显示，配合滚动
可以看全。

## 小终端布局

- 终端高度 ≥ 14 行时显示键位提示（2 行正文 + 边框）；更矮时整块隐藏；
- 内容区（设备列表 / 详情）固定至少保留 6 行；
- 状态栏高度取「消息实际渲染行数」，但不超过 `终端高度 - 1（标题）- 键位提示 -
  6（内容区）` 与 8 行的较小值，最低 3 行。

## 挂载与卸载

| 动作 | `udisksctl`（默认） | `sudo`（opt-in） |
|---|---|---|
| 挂载 | `udisksctl mount -b /dev/X`；成功后在状态栏报告真实挂载点 | `sudo mount -o <options> /dev/X <root>/<label-or-name>`，交接终端 |
| 卸载 | `udisksctl unmount -b /dev/X`；`busy` 时列出占用进程并提示 `f` | `sudo umount /dev/X`，交接终端 |
| 强制卸载 | 二次确认后 `udisksctl unmount -b /dev/X -f`（不中断界面） | 二次确认后 `sudo umount -l /dev/X`（交接终端） |
| 失败 | 只在状态栏报错，**不提权、不创建目录** | 状态栏报错；本次新建的目标目录（为空）被删除 |

sudo 模式的挂载点和选项：

- **名字**：有卷标用卷标（去掉首尾空白，`/`、`\` 与控制字符替换为 `_`，空 / `.` /
  `..` 回退到设备名），无卷标用设备名（如 `sdb1`）；
- **冲突**：目标已存在时，空目录直接复用；非空目录、普通文件、别的设备的挂载点
  依次尝试 `<base>-<name>`、`<base>-2`、`<base>-3`…；**符号链接直接拒绝**；
- **创建与清理**：目标目录只在交接终端前一刻创建；本次新建且挂载失败/取消时，
  若目录仍为空则删除。挂载根本身不在启动时创建，默认模式完全不涉及目录；
- **选项**：基础 `nosuid,nodev`（防止外来文件系统在主机上生效 setuid 二进制或设备
  节点）；`vfat` / `exfat` / `ntfs` / `ntfs3` / `iso9660` 追加
  `uid=<uid>,gid=<gid>,umask=0022`（取当前用户 `$HOME` 的属主，让这些文件系统对
  普通用户可写）。udisksctl 模式的挂载选项由 udisks2 决定。

**sudo 模式的代价**：每次挂载/卸载都要让出终端输密码；挂载点固定在配置的根目录
下，不会出现在 `/media`；也没有 udisks2 的设备授权与自动命名。

## 强制卸载

`f` 是破坏性操作（强制卸载只是把挂载点从命名空间摘掉，仍占用设备的进程在退出前
可能继续写入，存在丢数据风险），因此需要**二次确认**：

- 第一次 `f` 先做与 `u` 相同的前置检查（已挂载 / 非系统盘 / 单挂载点），通过后
  状态栏提示 `Press f again to force-unmount <name> (lazy unmount may lose data)`，
  此时不会执行任何命令；
- 同一设备再按一次 `f` 才真正执行；中途移动选择、翻页、折叠、`m` / `u` / `Enter`
  或 `r` 刷新都会取消这次确认，提示也随之消失；自动刷新若把该设备刷掉（拔出），
  确认同样失效并回落 `Ready`，不会把提示指向别的设备；
- 前置检查不通过时第一次即拒绝，不会进入等待确认状态；
- 实际命令随模式不同：默认模式 `udisksctl unmount -b <dev> -f`（不需要 sudo，界面
  不中断），sudo 模式 `sudo umount -l <dev>`（失败时再对挂载点重试一次）。

## 系统盘保护

卸载前会做三项检查，任何一项不通过都直接拒绝，**不会**执行 `udisksctl` 或
`sudo`：

1. 设备必须已挂载；
2. 设备的**任意**挂载点属于系统路径即拒绝：
   - 精确匹配：`/`、`/boot`、`/home`、`/usr`、`/var`、`/etc`、`/opt`、`/root`、`/srv`、`/tmp`；
   - 前缀匹配：`/boot/`、`/usr/`、`/var/`、`/etc/`、`/opt/`、`/srv/`；
   - `/home/<user>`（恰好两段）视为用户主目录，同样保护；
   - 挂载根之下的路径（如 `~/mnt/usb/<name>`）不受保护（本程序的挂载点要能卸）。
3. 多个挂载点（bind mount 等）只允许人工处理，见下节。

## 多挂载行为

一个设备可能有多个挂载点（`lsblk` 的 `mountpoints` 数组）。规则：

- 只要**任意**一个挂载点是系统路径，整台设备受保护（不会因为另有一个无害挂载点而放松）；
- 非系统设备若挂载点数量 > 1，`u` / `f` 一律拒绝并提示
  `has N mount points; unmount manually (e.g. sudo umount <mp>)`，避免半卸载；
- 列表里显示首个挂载点，多余的数量以 `(+N)` 标注，详情页逐行列出全部挂载点。

## 自动刷新

主循环每 2 秒轮询一次按键；空闲超时即静默执行一次 `lsblk`：

- 设备树有变化才重绘（热插拔即插即见），选中项按设备名保持；
- 刷新失败完全静默，不会覆盖状态栏里已有的挂载/卸载结果；
- 手动 `r` 刷新成功显示 `Refreshed`（替换掉旧的状态文字），失败则显示
  `Refresh failed: ...`。

## 测试

单元测试（不触发真实系统命令，全部走注入的 fake runner；挂载点/配置文件都用临时
目录，不碰真实 `$HOME`）：

```bash
cargo test
```

端到端回归（mock `lsblk`/`udisksctl`/`sudo` + `tmux`，状态写在临时目录，不碰真实
设备）：

```bash
cargo build
tests/e2e/run-all.sh
```

覆盖系统盘保护、多挂载拒绝、USB 挂载/卸载、LVM 成员提示、滚动、标签、热插拔
自动刷新、旧版 `lsblk` 回退、默认模式失败不提权不建目录、默认模式强制卸载
（二次确认 → `udisksctl unmount -f`）、sudo 模式的目标/选项/失败清理、卸载、
再次挂载与强制卸载成功报告、loop 过滤、实时搜索与详情滚动，以及非交互 CLI
（`list`/`status` 的人类与 JSON 输出、设备名/路径/卷标解析、退出码 0/1/2、
mount/unmount/force-unmount、默认模式零提权零目录、sudo 模式的目标与失败清理）。
详见 [`tests/e2e/README.md`](tests/e2e/README.md)。

## 已知限制

- **udisksctl 模式的挂载点不可配置**：`udisksctl mount` 不支持指定目标目录，挂载点
  由 udisks2 决定；想要固定位置只能用 sudo 模式。
- **sudo 模式需要交互认证**：每次挂载/卸载都要输密码，无法在无人值守场景使用。
- **旧目录不自动清理**：更早的版本会在 `$HOME/mnt/<name>` 建空目录，升级后不会主动
  删除，需要时自行 `rmdir`。
- **旧版 `lsblk` 的多挂载盲区**：缺少 `MOUNTPOINTS` 列时只能读到单个
  `MOUNTPOINT`，因此「多挂载点拒绝」在该环境下无法触发（保护降级为单点判断）。
- **真实 `sudo` / 热插拔仍需人工验证**：自动化只用 mock，真实 udisks2 行为、
  密码交互与物理插拔需在真机走查。
- **状态栏最多 8 行**：超长消息会被截断（尾部可能看不到），这是为了让设备列表
  保留可读空间。
- **键位提示是单行**：终端宽度不足以放下整行时，提示会从尾部裁剪（对应按键仍然
  可用，完整列表见上表）。
- **命令同步执行**：`udisksctl` 挂载/卸载期间界面短暂冻结（后台执行是后续版本的
  内容）。
- **CLI 每次调用都重新读取设备**：非交互子命令不维护缓存，也不做后台自动刷新；
  热插拔后重新执行一次命令即可。
- **JSON 字段只追加**：`list --json` / `status --json` 的字段名不会改名或删除，
  新字段只会追加，因此已有脚本不会因为升级而失效。
- 不处理 LUKS 解锁、`fstab` 编辑与挂载选项。
