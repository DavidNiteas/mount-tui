# mount-tui 端到端回归（mock）

这里的脚本用 **mock 的 `lsblk` / `udisksctl` / `sudo`** + `tmux` 驱动真实的
`mount-tui` 二进制，覆盖单元测试难以触达的交互路径：

| 场景 | 覆盖内容 | 断言数 |
|---|---|---|
| `run_main.sh` | 启动界面、标签、系统盘保护（卸载/强制卸载均拒绝且不调用任何命令）、USB 挂载/卸载、LVM 成员不可挂载提示、`/home/<user>` 保护、多挂载点拒绝、`End`/`Home` 滚动、`q` 退出 | 30 |
| `run_hotplug.sh` | 2 秒轮询自动刷新：设备出现/消失无需按键，选中项与状态栏不被刷新覆盖 | 9 |
| `run_legacy.sh` | 旧版 `lsblk`（无 `MOUNTPOINTS`）自动回退到旧列重试，`MOUNTPOINT`/`RO`/`RM` 正确解析 | 8 |
| `run_sudo.sh` | `udisksctl` 失败后交接给 `sudo`（终端让出/恢复、Enter 继续），CJK 长错误尾部 `END_OF_LONG_ERROR` 不被裁剪 | 8 |

合计 55 项断言；`run-all.sh` 聚合四个场景，全部 PASS 才 `exit 0`。

## 依赖

- Linux + `tmux`（`tmux -V` 能跑即可）
- 已构建的调试二进制：`cargo build`（本仓库开发环境可加 `--offline`）
- 不需要 root、不需要真实块设备、不会真的挂载/卸载任何东西

## 运行

```bash
cargo build
tests/e2e/run-all.sh
```

单个场景：

```bash
tests/e2e/run_main.sh
```

指定二进制（例如 release 构建）：

```bash
MOUNT_TUI_BIN=target/release/mount-tui tests/e2e/run-all.sh
```

## 设计要点

- **不写仓库**：每个场景用 `mktemp -d` 建临时目录，state（`mounted`、`hotplug`、
  调用日志）全部放在 `$MOCK_STATE` 下，`trap` 在退出时删除。mock 脚本只通过
  `MOCK_STATE` 环境变量读写状态。
- **`HOME` 隔离**：TUI 以临时目录作为 `$HOME` 启动，`sudo mount` 的兜底挂载点
  （`$HOME/mnt/<name>`）也落在临时目录里。
- **系统命令全部 stub**：`lsblk`、`udisksctl`、`sudo`、`fuser`、`lsof` 都来自
  `mock/bin`（旧列场景额外前置 `mock_legacy/bin`），mock 之外的命令不会被调用。
- **session 隔离**：tmux session 名带 PID（`mtui_<scenario>_$$`），并行跑多个
  场景不会互相踩。
- **二进制缺失**：提示先 `cargo build --offline` 并 `exit 1`。

## 限制

- mock 不是真实挂载：它只模拟 `udisksctl`/`lsblk` 的输出与状态文件，验证的是
  TUI 的解析、决策与渲染，不验证 udisks2 的真实行为。`fuser` / `lsof` 同样被
  stub（记录调用、报告「无进程」并失败退出），busy 诊断路径也永远不会碰到真实
  命令。
- 真实 `sudo` 密码交互、真实 USB 热插拔仍需人工在真机上走查。
- 断言基于 `tmux capture-pane` 的文本快照，依赖终端尺寸（默认 120×40）；可用
  `E2E_COLS` / `E2E_ROWS` 覆盖，但改动尺寸需要同步调整期望文案的换行。
