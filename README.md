# Mount-TUI

终端里的磁盘与 USB 管理器：列出块设备树，挂载 / 卸载可移动介质，并对系统盘做
保护。界面左右两栏——左侧设备树（含分区、LVM、crypt 子节点），右侧选中设备的
详情（型号、传输方式、文件系统、卷标、UUID、挂载点、是否受保护），底部是状态栏
与键位提示。

状态栏按实际渲染行数自动增高（上限 8 行）：8 行以内的 CJK 长错误不会被截断，
超过上限仍会裁剪。

## 依赖

- Linux（读取 `/proc`/`lsblk` 的块设备信息）
- `lsblk`（util-linux）；旧版本缺少 `MOUNTPOINTS` 列时会自动回退到旧列
- `udisksctl`（udisks2）优先；不可用时回退到 `sudo mount` / `sudo umount`
- `fuser` / `lsof`（可选）：卸载遇 `busy` 时用于列出占用进程
- `tmux`：仅端到端测试需要

## 构建

```bash
cargo build
```

产物：`target/debug/mount-tui`。Rust 依赖只有 ratatui / crossterm / serde /
serde_json / anyhow；系统命令依赖见上一节。本仓库开发环境依赖已缓存，使用
`--offline` 也可构建。

## 运行

```bash
./target/debug/mount-tui
```

启动时先读取设备列表，读取失败会在 TUI 状态栏里显示原因，而不是直接退出。
运行期出现无法恢复的错误（例如终端初始化失败）时，程序先恢复终端，再以**非零
退出码**结束——脚本可以据此判断成败。

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
| `f` | 强制惰性卸载（`umount -l`）：第一次按下只提示确认，同一设备再按一次才执行 |
| `r` | 立即刷新设备列表 |
| `q` / `Esc` / `Ctrl-C` | 退出 |

挂载优先调用 `udisksctl mount -b /dev/X`；失败时把终端让给 shell，执行
`sudo mount -o nosuid,nodev /dev/X $HOME/mnt/X`（挂载点由本程序创建），完成后按
Enter 返回 TUI，状态栏按真实挂载点报告结果。`nosuid,nodev` 防止外来文件系统
在主机上生效 setuid 二进制或设备节点；`udisksctl` 路径的挂载选项由 udisks2 决定。
卸载同理：`udisksctl unmount` 失败后交接 `sudo umount`。

## 强制卸载

`f` 是破坏性操作（`umount -l` 只是把挂载点从命名空间摘掉，仍占用设备的进程在
退出前可能继续写入，存在丢数据风险），因此需要**二次确认**：

- 第一次 `f` 先做与 `u` 相同的前置检查（已挂载 / 非系统盘 / 单挂载点），通过后
  状态栏提示 `Press f again to force-unmount <name> (lazy unmount may lose data)`，
  此时不会执行任何命令；
- 同一设备再按一次 `f` 才真正执行；中途移动选择、翻页、折叠、`m` / `u` / `Enter`
  或 `r` 刷新都会取消这次确认，提示也随之消失；
- 前置检查不通过时第一次即拒绝，不会进入等待确认状态。

## 系统盘保护

卸载前会做三项检查，任何一项不通过都直接拒绝，**不会**执行 `udisksctl` 或
`sudo`：

1. 设备必须已挂载；
2. 设备的**任意**挂载点属于系统路径即拒绝：
   - 精确匹配：`/`、`/boot`、`/home`、`/usr`、`/var`、`/etc`、`/opt`、`/root`、`/srv`、`/tmp`；
   - 前缀匹配：`/boot/`、`/usr/`、`/var/`、`/etc/`、`/opt/`、`/srv/`；
   - `/home/<user>`（恰好两段）视为用户主目录，同样保护；
   - `$HOME/mnt/<name>` 这类更深路径不受保护（本程序的挂载点要能卸）。
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

单元测试（不触发真实系统命令，全部走注入的 fake runner）：

```bash
cargo test
```

端到端回归（mock `lsblk`/`udisksctl`/`sudo` + `tmux`，状态写在临时目录，不碰真实设备）：

```bash
cargo build
tests/e2e/run-all.sh
```

覆盖系统盘保护、多挂载拒绝、USB 挂载真实挂载点、LVM 成员提示、滚动、标签、
热插拔自动刷新、旧版 `lsblk` 回退、`sudo` 终端交接与 CJK 长错误尾部。详见
[`tests/e2e/README.md`](tests/e2e/README.md)。

## 已知限制

- **旧版 `lsblk` 的多挂载盲区**：缺少 `MOUNTPOINTS` 列时只能读到单个
  `MOUNTPOINT`，因此「多挂载点拒绝」在该环境下无法触发（保护降级为单点判断）。
- **真实 `sudo` / 热插拔仍需人工验证**：自动化只用 mock，真实 udisks2 行为、
  密码交互与物理插拔需在真机走查。
- **状态栏最多 8 行**：超长消息会被截断（尾部可能看不到），这是为了让设备列表
  保留可读空间。
- **命令同步执行**：`udisksctl` 挂载/卸载期间界面短暂冻结（后台执行是后续版本的
  内容）。
- 不处理 LUKS 解锁、`fstab` 编辑与挂载选项。
