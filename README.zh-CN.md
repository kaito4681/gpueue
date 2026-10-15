<a id="puzuz"></a>
# puzuz

[English](README.md) | [日本語](README.ja.md) | 简体中文

面向多用户共享的单台 Linux 机器的 GPU 作业队列。其 CLI 的用法与
[pueue](https://github.com/Nukesor/pueue) 相同。puzuz 在此基础上增加了两项功能：GPU 槽位，以及借助
[CRIU](https://criu.org) 和 NVIDIA [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint)
对运行中的作业（CPU、内存和 GPU 状态）进行透明的检查点保存与恢复。机器重启时仍在运行的训练会被保存到磁盘，
并从中断处继续运行，而不必从头开始。

> **状态：预发布。** 下文所述的全部功能均已实现，并已在 Docker 中测试（systemd 作为 PID 1、
> 真实的 CRIU、真实的 NVIDIA GPU），并在运行 CPU 作业的 QEMU/KVM 虚拟机的真实重启和关机中测试过。
> 但尚**未**在物理机（裸机）上或带着 GPU 作业经历真实重启的测试。
> 参见[项目状态](#project-status)。

> **尚未发布任何版本。** 下面的下载链接、APT 仓库和 `stable` 分支将从第一个版本发布起可用。

<a id="why"></a>
## 为什么需要 puzuz

在共享的 GPU 工作站上，人们通常用 `nohup`、`tmux` 或每个用户各自的 pueue 来运行作业，并在聊天群里争抢 GPU。
为了更新内核或驱动而重启，会终止所有正在进行的训练。puzuz 提供：

- **所有人共用一个队列**：一个系统级守护进程调度所有用户的作业，并显示谁在使用哪块 GPU，
- **GPU 槽位**：作业申请指定的 GPU（`--gpu 0,1`），并且只在这些 GPU 空闲时才启动，
- **检查点/恢复**：在操作系统关机时、执行 `puzuz shutdown` 时，或通过 `puzuz checkpoint` 按需保存运行中的作业。
  之后用 `puzuz restore` 继续运行，无需修改你的代码。

<a id="features"></a>
## 功能

- 兼容 pueue 的命令：`add`、`status`、`log`、`follow`、`wait`、`pause`、`start`、`kill`、
  `remove`、`restart`、`clean`、`reset`、`stash`、`enqueue`、`switch`、`group`、`parallel`、
  `shutdown`、`completions`，以及组、依赖（`--after`）、优先级、延迟和标签。
- GPU 槽位：按 GPU 限制同时运行的作业数（`puzuz gpu parallel`）。每个作业会获得
  `CUDA_VISIBLE_DEVICES` 和 `CUDA_DEVICE_ORDER=PCI_BUS_ID`。GPU 通过 NVML 检测。
- 用户之间的公平共享：Slurm 风格的多因子优先级（带衰减用量的 fair share、
  等待时间、nice、`top`；`puzuz prio` 和 `puzuz share` 显示具体数值）。
- 对 CPU 和 CUDA 进程进行透明的检查点保存与恢复，包括 DataLoader worker 通过 fork 创建的 PyTorch 作业
  （参见 [fork guard](#limitations)）。
- 操作系统关机或重启时，自动为所有运行中的作业创建检查点。
- 多用户：作业以其所有者的身份运行。其他用户可以看到作业的存在，但看不到其命令、环境变量或日志。
  管理员是 root 和 `puzuz-admin` 组的成员。
- 权限分离：守护进程（HTTP API、调度器、SQLite）以非特权系统用户 `puzuz` 运行。只有一个小型的 root helper
  负责启动作业和运行 CRIU。
- 作业运行在各自的 systemd 单元中，因此守护进程或 helper 重启时（例如升级期间）作业会继续运行。
- `puzuz doctor` 检查安装情况，并针对每个问题打印修复命令。

<a id="quick-start"></a>
## 快速开始

要求和详细说明参见 [docs/zh-CN/install.md](docs/zh-CN/install.md)。

<a id="install-administrator"></a>
### 安装（管理员）

在带有 systemd 且 glibc 为 2.34 或更新版本的 x86_64 或 aarch64 Linux 上（Ubuntu 22.04+、Debian 12+、
RHEL 9+），安装脚本会下载最新的发布版本、进行校验，并将 puzuz 安装到
/usr/local/bin：

```sh
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

安装脚本会设置 systemd 单元、`puzuz` 用户、`puzuz-admin` 组、各个目录以及默认的
/etc/puzuz/config.toml，启动服务，并打印 `puzuz doctor` 的报告。它不会安装 CRIU 或 cuda-checkpoint；
如果它们缺失，脚本会提示你。之后用 `sudo puzuz update` 更新（运行中的作业会继续运行）。用
`sudo puzuz uninstall` 卸载，数据会被保留（加上 `--purge` 则一并删除）。不需要 GitHub token（如果设置了
`GH_TOKEN` 则会使用它；它只用于提高 API 速率限制）。

在 Debian 和 Ubuntu（amd64 或 arm64）上，也可以改为从 puzuz 的 APT 仓库（位于 GitHub Release `apt` 中）
安装软件包；这需要 `curl` 和 `ca-certificates`，最新的稳定版本和架构由 apt 自动选择：

```sh
sudo curl -fsSLo /etc/apt/keyrings/puzuz.gpg https://github.com/kaito4681/puzuz/releases/download/apt/puzuz.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/puzuz.sources https://github.com/kaito4681/puzuz/releases/download/apt/puzuz.sources
sudo apt update && sudo apt install puzuz
```

或者直接安装最新发布版本的 `.deb`（它会注册同一个仓库）：

```sh
arch=$(dpkg --print-architecture)
curl -fLO "https://github.com/kaito4681/puzuz/releases/latest/download/puzuz_${arch}.deb"
sudo apt install "./puzuz_${arch}.deb"
```

它会安装并启动相同的服务，并推荐安装 `criu`（前提条件参见
[docs/zh-CN/install.md](docs/zh-CN/install.md#installing-the-debianubuntu-package)）。无论哪种方式，之后的版本
都通过 `sudo apt update && sudo apt upgrade` 获取。`sudo apt remove puzuz` 会停止服务；`sudo apt purge puzuz`
还会删除 /etc/puzuz。两者都会保留 /var/lib/puzuz 和运行中的作业。

在 Fedora、RHEL、Rocky 和 Alma Linux 9+（x86_64 或 aarch64）上，安装发布版本中的 .rpm
（dnf 可以直接接受 URL）：

```sh
sudo dnf install "https://github.com/kaito4681/puzuz/releases/latest/download/puzuz.$(uname -m).rpm"
```

这些 URL 始终指向最新的稳定发布版本（`releases/latest` 会跳过预发布版本）；带版本号的文件
（`puzuz_<version>_<arch>.deb`、`puzuz-<version>-1.<arch>.rpm`）也一并附带，用于安装特定版本。
每个文件都有 `.sha256` 和 `.minisig` 文件（参见
[手动验证发布版本](docs/zh-CN/install.md#verifying-a-release-by-hand)）。

目前还没有 dnf 仓库（已在计划中，issue #23）：更新时用 `sudo dnf upgrade` 运行同一条命令。`sudo dnf remove puzuz` 会停止服务，并保留 /var/lib/puzuz 和运行中的作业。它推荐安装 `criu`；
Fedora 的版本足够新，而 Rocky/RHEL 9 只提供 3.19，因此在那里创建检查点需要更新的 CRIU（没有它作业也能运行）。
你也可以从源码构建，或使用 Nix flake（参见 [docs/zh-CN/install.md](docs/zh-CN/install.md)）。

在 NixOS 上，导入 flake 的模块（`github:kaito4681/puzuz/stable`、`nixosModules.default`）并设置
`services.puzuz.enable = true`（参见 [NixOS](docs/zh-CN/install.md#nixos)）；home-manager 用户可以通过
`homeManagerModules.default` 添加 CLI 的补全。在其他发行版上使用 Nix（x86_64 或 aarch64）时，构建该 flake，
然后从构建结果安装服务（它会被复制到 /usr/local/bin，并且不会被 Nix 的垃圾回收清除；参见
[使用 Nix 安装](docs/zh-CN/install.md#installing-with-nix)）：

```sh
nix build github:kaito4681/puzuz/stable
sudo ./result/bin/puzuz service install
```

在 Arch Linux（x86_64、aarch64）上，目前请使用上面的安装脚本：AUR 软件包 `puzuz-bin` 尚未发布。
CRIU（4.2 或更新版本，用于检查点）位于官方仓库中：

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

用 `sudo puzuz update` 更新，用 `sudo puzuz uninstall` 卸载。软件包发布到 AUR 后，你也可以改用任意 AUR
helper 安装 [`puzuz-bin`](https://aur.archlinux.org/packages/puzuz-bin)（参见
[docs/zh-CN/install.md](docs/zh-CN/install.md#arch-linux-aur)）。

```sh
# Check everything: criu, cuda-checkpoint, driver, kernel, systemd, services, storage.
puzuz doctor

# Optional: make someone an administrator.
sudo usermod -aG puzuz-admin alice
```

普通用户无需安装任何东西。

<a id="run-a-gpu-job-any-user"></a>
### 运行 GPU 作业（任意用户）

```sh
# Queue a job on GPU 0. The command runs through `sh -c` in the current directory, with your
# environment.
puzuz add --gpu 0 -l resnet -- 'python train.py --epochs 100'

puzuz status          # all jobs of all users (others' commands are hidden)
puzuz gpu             # GPUs, slots in use, and which jobs hold them
puzuz log 0           # the last lines of job 0's output
puzuz follow 0        # stream the output, like tail -f
```

<a id="checkpoint-and-restore"></a>
### 检查点与恢复

```sh
puzuz checkpoint 0    # save job 0 to disk and stop it; frees the GPU. State: Checkpointed
puzuz restore 0       # queue it again; it resumes once its group and GPUs have a free slot
puzuz restore -i 0    # ...or put it first among your queued jobs (administrators: resume right away; see "Job priority")
```

恢复后的作业从创建检查点的那一刻继续运行。它保留原来的作业 id，其输出会追加到同一个日志中。

暂停的作业同样可以创建检查点，重启时也是如此。它恢复后仍处于暂停状态，并继续占用其 GPU 槽位，用
`puzuz start` 即可让它继续运行。CRIU 和 cuda-checkpoint 需要一个正在运行的进程，因此在转储期间作业会运行片刻。
恢复会让它保持停止状态，因此在 `puzuz start` 之前它不会执行任何代码（哪怕片刻）。检查点正在进行时，
`puzuz pause` 会被拒绝。

<a id="what-happens-on-reboot"></a>
### 重启时会发生什么

1. 操作系统关机时，`puzuz-helper.service` 会在文件系统和 NVIDIA 服务之前停止。它逐个为运行中的作业创建检查点。
   `--no-checkpoint` 作业则直接被终止。
2. 启动后，这些作业显示为 `Checkpointed`。原本在排队的作业继续排队，并照常启动。
3. 每个所有者（或管理员）用 `puzuz restore <id>` 或 `puzuz restore -a` 恢复自己的作业。恢复不会自动开始。

检查点可能以两种方式出错：

- **被拒绝：** 在触碰作业之前，helper 的检查就失败了（可用空间不足、存在一个永远无法恢复的挂载等）。
  作业继续运行。`puzuz checkpoint` 会失败并给出原因。如果拒绝发生在应当转储的时刻（例如在 `puzuz shutdown`
  期间），所有者会在下一次执行 `puzuz` 命令时收到警告。原因会作为作业的最后一条错误显示在
  `puzuz status` 和 `puzuz log` 中。
- **失败：** 转储本身失败。CRIU 会让作业继续，但作业已经被冻结过，因此会被停止（先发送 SIGTERM，
  让它有机会保存自己的检查点，`grace_secs` 之后再发送 SIGKILL），并显示为 `Checkpoint failed` 及其原因。
  所有者会在下一次执行 `puzuz` 命令时收到警告。

在操作系统关机或重启期间，systemd 会停止所有无法保存的作业，无论其转储是被拒绝还是失败。启动后，它会显示为
`Checkpoint failed` 及其原因，并且所有者会收到警告。断电或内核崩溃时不会保存任何内容。

<a id="pueue-compatibility-and-differences"></a>
## pueue 兼容性与差异

命令名、选项和表格布局遵循 pueue 4.0，因此把 `pueue` 换成 `puzuz` 后，大多数 pueue 的使用习惯和脚本都能继续使用。
puzuz 只复制了 CLI。它不使用 pueue 的协议，也不读取 pueue 的文件。差异如下：

| 项目 | pueue | puzuz |
| --- | --- | --- |
| 守护进程 | 每个用户一个，由用户启动（`pueued`） | 整个系统一个，由 systemd 运行（`puzuz daemon`，用户 `puzuz`）。用户从不启动守护进程。 |
| 选择守护进程 | `-c/--config`、`-p/--profile` | `--socket <path>`（环境变量 `PUZUZ_SOCKET`，默认 `/run/puzuz/puzuz.sock`） |
| `send` | 向任务的 stdin 发送输入 | 不支持：作业的 stdin 是 `/dev/null`。请让命令以非交互方式运行。 |
| `shutdown` | 终止所有任务并停止守护进程 | 仅限管理员。为所有运行中的作业创建检查点，然后停止守护进程。检查点被拒绝的作业会继续运行。`--no-checkpoint` 则改为终止它们。 |
| `restart` | 已结束的任务 | 也适用于 `Checkpointed` 作业：它们作为新作业从头开始，镜像会被保留 |
| `start <id>`、`add -i`、`restart -i`（以及 `restore -i`） | 忽略所有限制，立即启动任务 | 只有管理员的会忽略所有限制立即启动；其他用户的作业会被排到其自己作业的最前面（与 `puzuz top` 相同），并像其他排队作业一样等待轮到它（参见 [作业优先级](#job-priority)） |
| `reset`、`-a/--all`、`-g/--group` | 所有任务 | 你自己的作业。管理员作用于所有人的作业。 |
| `pause/start/kill -a`、`-g` | 同时暂停/继续这些组 | 只有管理员会暂停/继续组；对其他用户而言，组保持原样 |
| 用语 | "task" | "job"。为了兼容，保留了 `--print-task-id` 等选项名。 |
| `--json` | pueue 的内部状态 | puzuz 自己的格式，见下文 |
| `edit`、`env`、`restart --edit` | 支持 | 接受但尚未实现 |
| `add -o/--priority`、`switch` | 对守护进程的所有任务排序 | 只对你自己的作业排序；哪个用户的作业先启动由带 fair share 的 Slurm 风格多因子优先级决定（参见[作业优先级](#job-priority)）。`priority_type = "basic"` 可恢复 pueue 的顺序。 |
| 回调、别名、`pueue.yml` | 支持 | 不可用 |

额外的命令和选项：

| 命令 | 用途 |
| --- | --- |
| `add --gpu <LIST>` | 作业运行所用的 GPU，例如 `0` 或 `0,1`。不指定时作业仅使用 CPU（`CUDA_VISIBLE_DEVICES` 为空）。 |
| `add --no-checkpoint` | 从不为该作业创建检查点；关机时将其终止 |
| `checkpoint [IDS] [-g GROUP] [-a]` | 保存运行中的作业并停止它们 |
| `restore [IDS] [-g GROUP] [-a] [-i]` | 继续运行 `Checkpointed` 作业（`-i`：与 `start` 一样，管理员立即运行，其他用户排到自己作业的最前面） |
| `gpu [-j]`、`gpu parallel <N> [--gpu LIST]` | 列出 GPU，或设置每块 GPU 的作业数上限（管理员） |
| `add --nice <N>`、`nice <IDS>... <N>` | 将作业的优先级降低 `N`（Slurm 的 nice；负值仅限管理员） |
| `top <IDS>...` | 将作业移到你自己队列的最前面（Slurm 的 `scontrol top`） |
| `prio [IDS] [-j]` | 按调度顺序列出等待中的作业及其优先级因子（类似 Slurm 的 `sprio`） |
| `share [-j]`、`share set <USER> <N>` | 每个用户的 fair-share 状态（类似 Slurm 的 `sshare`）；设置用户的份额（管理员） |
| `site <IDS>... <N>`、`group priority <GROUP> <N>` | 作业的 site 因子、组的优先级（管理员） |
| `service install [--bin-dir DIR] [--no-start] [--force] / uninstall / status` | 管理 systemd 服务；如果最终检查失败，install 以非零状态退出（参见 [docs/zh-CN/install.md](docs/zh-CN/install.md)） |
| `uninstall [--purge] [--yes]` | 删除服务以及 `service install` 复制的二进制文件（需要 root） |
| `update [--check] [--version V] [-y]` | 将 puzuz 更新到最新（或指定的）发布版本；需要 root，非 root 时只检查（参见 [docs/zh-CN/install.md](docs/zh-CN/install.md#upgrading)） |
| `doctor` | 检查安装情况 |
| `daemon` | 在前台运行守护进程（由 systemd 执行） |

额外的作业状态：`Checkpointing`、`Checkpointed`、`Restore queued`、`Restoring` 以及
`Terminating`（检查点失败后正在停止）。`status` 表格包含 `Owner` 和 `GPU` 列，`Checkpointed` 作业会显示镜像大小。

<a id="json-output"></a>
### JSON 输出

`puzuz status --json` 输出 `{"groups": [...], "jobs": [...]}`：

```json
{
  "groups": [{"name": "default", "parallel": 1, "paused": false}],
  "jobs": [{
    "id": 0, "owner": 1000, "group": "default", "gpus": [0], "dependencies": [],
    "priority": 0, "label": "resnet", "no_checkpoint": false,
    "state": "done", "result": "failed", "exit_code": 1,
    "created_at": 1760000000000, "started_at": 1760000001000, "finished_at": 1760000100000,
    "checkpoint_size_bytes": 8100000000,
    "details": {"command": "python train.py", "cwd": "/home/alice/proj", "last_error": "..."}
  }]
}
```

- 时间戳是 Unix 纪元毫秒数。`owner` 是 uid。
- `state` 的字段被展开到作业对象中。`state` 取以下值之一：`queued`
  （`enqueued_at`）、`stashed`（`enqueue_at` 或 null）、`running`、`paused`、`checkpointing`、
  `checkpointed`、`restore_queued`（`enqueued_at`）、`restoring`、`terminating` 和 `done`。
  `terminating` 和 `done` 带有 `result`，其值为以下之一：`success`、`failed`（`exit_code`）、
  `failed_to_spawn`（`reason`）、`killed`、`dependency_failed`、`checkpoint_failed`（`reason`）和
  `lost`。
- `checkpoint_size_bytes`、`details`、`details.last_error`、作业的 `nice` 以及组的
  `priority` 在不适用（或为 0）时省略。
  对于其他用户的作业，`label` 为 null，`details` 不存在，`reason` 为 `"<redacted>"`。
- `waiting` 只出现在由非管理员立即启动、但正在等待 GPU 槽位或其暂停的组的排队作业上：
  `{"reason": "gpu_slots", "gpus": [0]}`、`{"reason": "group_paused"}` 或 `{"reason": "unavailable"}`。
- `puzuz log --json` 输出 `{"<id>": {"job": <job>, "output": "..."}}`。`puzuz group --json`
  输出 `{"groups": [...]}`。`puzuz gpu --json` 输出
  `{"gpus": [{"index", "uuid", "name", "memory_total_bytes", "driver_version", "parallel",
  "used", "jobs": [{"id", "owner"}]}]}`，其中 `"available": false` 表示已消失的 GPU。
- `puzuz prio --json` 输出 `{"priority_type", "weights": {"age", "fairshare", "job_size",
  "partition", "tres"}, "jobs": [{"id", "owner", "group", "priority", "factors": {"age",
  "fairshare", "job_size", "partition", "tres_cpu", "tres_gpu"}, "weighted": {"age", "fairshare",
  "job_size", "partition", "tres"}, "site", "nice"}]}`，作业按调度顺序排列。
  `puzuz share --json` 输出 `{"users": [{"uid", "shares", "norm_shares", "raw_usage",
  "effective_usage", "fairshare", "active"}], "decay_half_life_secs", "calc_period_secs",
  "usage_reset_period", "billing_weights", "usage_calc_at"}`。

<a id="multi-user-model-and-authorization"></a>
## 多用户模型与授权

- 守护进程通过内核报告的套接字对端 uid（`SO_PEERCRED`）识别调用者。客户端无法冒充他人。
- 每个作业属于添加它的用户。它以该用户的 uid、gid 和附加组运行，设置了 `no_new_privs`，并丢弃全部 capability。
  命令在添加作业时所在的目录中运行，使用 `puzuz add` 调用时的环境变量。
- root helper 拒绝以下用户的作业：系统账户（uid 小于 `job_min_uid`，默认为 `/etc/login.defs` 的
  `UID_MIN`）、设置了 `job_allowed_users` 时不在其中的用户，以及主组为等同 root 的组的用户
  （`job_denied_groups`，默认为 `docker`、`lxd`、`incus-admin`、`libvirt`、`libvirtd`、`disk`、`sudo`、`wheel`、
  `admin`）。这些组会从每个作业的附加组中去掉，因此即使所有者能使用 Docker 或 sudo，作业也不能
  （helper 的日志会记录去掉了哪些组）。被拒绝的作业以 `puzuz refuses to run jobs as <user>: <reason>` 为原因启动失败。
  见 [谁可以运行作业](docs/zh-CN/install.md#who-jobs-may-run-as)。
- 即使所有者可以成为 root，作业也无法成为 root 或以 root 身份操作：在 `no_new_privs` 下 setuid 程序（`sudo`、
  `su`、`pkexec`）无法获得特权；作业单元无法访问系统 D-Bus，因此无论 polkit 规则授予所有者的组什么权限，
  都无法执行 `systemctl`、`run0`、`machinectl` 或其他经 polkit 授权的操作（`job_system_dbus = true` 可解除），
  也无法访问 `/run/systemd` 下经 polkit 授权的 systemd Varlink 服务（`job_systemd_varlink`），
  以及在用户数据库中检查所有者的组的守护进程套接字（`/run/libvirt` 中经 polkit 授权的 libvirt 套接字、
  `/run/cups` 中的 CUPS 套接字；`job_hidden_socket_dirs`）；
  等同 root 的组会被去掉；只能打开所分配 GPU 的设备节点。恢复之后同样如此。见
  [作业能做什么、不能做什么](docs/zh-CN/install.md#what-a-job-can-and-cannot-do)。
- **仅限所有者或管理员：** 更改作业（`kill`、`pause`、`start`、`remove`、
  `checkpoint`、`restore`、`restart`、`stash`、`enqueue`、`switch` 等），以及查看其命令、
  工作目录、环境变量、标签、失败原因和日志。同样，`add --after` 只接受调用者能够删除的作业
  （自己的作业；管理员则为任意作业）：一个作业会阻止删除它所依赖的作业，因此依赖他人的作业会使对方无法删除或清理该作业。
  重复给出的依赖只记录一次。
- **所有人：** 查看作业的存在、其所有者、组、GPU、状态、优先级、依赖和时间（`status`、`gpu`），
  等待中作业的优先级因子（`prio`）以及所有人的 fair-share 用量（`share`）。由此可以看到谁在使用哪块 GPU。
- **所有者：** 调高 `nice`（不能低于 0），以及 `top`（除非设置了 `enable_user_top = false`）。
- **仅限管理员：** `share set`、`group priority`、`site`、负值的 `nice`、对其他用户作业执行 `top`、
  `group add/remove`、`parallel`、`gpu parallel`、暂停或继续整个组
  （`pause/start/kill -g` 针对一个组，`pause/start/kill -a` 针对所有组）、
  `shutdown`、对其他用户作业执行 `reset`，以及对[孤立作业](docs/zh-CN/architecture.md#orphans)（helper 正在运行、
  但守护进程并不掌握的进程；`status` 会向管理员列出它们）执行 `kill <id>`。与 pueue 一样，`pause` 和 `kill` 会暂停所选的组，
  `start` 会继续它们，而 `pause --wait` 只暂停组，并让运行中的作业运行完毕。与 pueue 不同的是，
  即使组中没有排队的作业，`kill -g/-a` 也会暂停这些组。
  普通用户运行 `pause/start/kill -g` 或 `-a` 时，只有其自己的作业受影响，任何组的暂停状态都不会改变
  （此时 `pause --wait` 什么也不做）。输出会说明发生的是哪种情况
  （`All groups are being paused.` 或 `Your jobs are being paused: 3, 4`）。
- **只有管理员能立即启动作业**（`start <id>`、`add -i`、`restart -i`、`restore -i`），并忽略所有限制；
  其他用户的作业会被排到其自己作业的最前面（参见 [作业优先级](#job-priority)）。
- 管理员是 root 以及 `puzuz-admin` 组的成员（主组或附加组均可）。每次请求都会进行检查，因此
  `usermod -aG puzuz-admin <user>` 会立即生效。

<a id="gpu-slots"></a>
## GPU 槽位

- `puzuz add --gpu 0,1 ...` 只有在其组有空闲槽位且所列的**每一块** GPU 都有空闲槽位时才会启动作业。
  守护进程会拒绝它不认识的 GPU 编号。
- 默认情况下每块 GPU 同时只允许 1 个作业。管理员可以用
  `puzuz gpu parallel 2`（所有 GPU）或 `puzuz gpu parallel 2 --gpu 1` 更改。`0` 表示不限制。
  槽位按作业计数，而不是按显存。
- GPU 编号遵循 PCI 总线顺序，与 `nvidia-smi` 和
  `CUDA_DEVICE_ORDER=PCI_BUS_ID` 的顺序相同。在作业内部，`CUDA_VISIBLE_DEVICES` 列出所申请的
  GPU，因此程序看到的是 `cuda:0`、`cuda:1`……
- **GPU 隔离：** 作业只能打开分配给它的 GPU 的设备节点。每个作业单元都设置了 systemd 的
  `DevicePolicy=closed`，并用 `DeviceAllow=` 只允许所分配 GPU 的 `/dev/nvidia<N>` 和共用的 NVIDIA 节点；
  仅使用 CPU 的作业完全不能打开 NVIDIA 设备。修改 `CUDA_VISIBLE_DEVICES` 或打开其他 GPU 的节点都会以
  `EPERM` 失败，作业内的 `nvidia-smi` 只显示自己的 GPU（在仅使用 CPU 的作业中会以
  `Failed to initialize NVML: Unknown Error` 失败）。管理员可以用 `gpu_device_isolation = false` 关闭它，
  例如使用 MIG 时（见 [GPU 隔离](docs/zh-CN/install.md#gpu-isolation)）。
- 组的行为与 pueue 相同（`default` 组初始为 `parallel 1`）。两种限制同时生效。
- GPU 槽位、组的 `parallel` 限制和暂停的组适用于普通用户启动的每个作业，使用 `start <id>`、`add -i`、
  `restart -i` 或 `restore -i` 时也一样（参见 [作业优先级](#job-priority)）；只有管理员的即时启动会忽略它们。
- GPU 在守护进程启动时通过 NVML 检测（配置中的 `[[gpus]]` 列表只是后备）。添加或移除 GPU 后请重启
  `puzuz.service`。`puzuz doctor` 会将守护进程检测到的内容与 `nvidia-smi` 进行比较。

<a id="job-priority"></a>
## 作业优先级

当多个作业等待相同的槽位时，在能够放入的作业中优先级最高的那个先启动。优先级遵循 Slurm 的 multifactor
插件：默认情况下，它是 fair-share 因子（最近使用机器较少的用户优先，用量以 7 天的半衰期衰减）与
等待时间因子（等待的时长）之和，再减去作业的 nice 值。也可以加入作业大小、组（partition）和按 GPU（TRES）
的因子。你自己的 `-o` 优先级、`switch` 和 `puzuz top` 只会重新排列你自己的作业。非管理员使用 pueue 的
即时启动（对排队中或暂存作业的 `start <id>`、`add -i`、`restart -i`、`restore -i`）时也是如此：作业会像
`puzuz top` 一样被排到你自己作业的最前面，之后在组的 `parallel` 限制、GPU 槽位、暂停的组和依赖之下与其他
用户的作业竞争；命令会说明它是被排队而不是被启动。在 `enable_user_top = false`（或
`priority_type = "basic"`）时，作业只是被排队。对暂停作业执行 `start` 仍然是恢复它。管理员的即时启动会
忽略所有限制立即启动。`puzuz prio` 显示作业
为何处于当前位置，`puzuz share` 显示所有人的用量。公式和设置参见
[优先级与公平共享](docs/zh-CN/install.md#priority-and-fair-share)。

<a id="requirements"></a>
## 要求

- 带有 systemd 240 或更新版本的 Linux（已测试：259）。发布版本针对 x86_64 和 aarch64 构建；只有
  x86_64 在真实硬件上测试过。预构建的二进制文件需要 glibc 2.34 或更新版本。
- 启用了 `CONFIG_CHECKPOINT_RESTORE` 的内核，推荐 5.5 或更新版本（已测试：7.0）。
- CRIU 4.2 或更新版本。发行版软件包足够新时即可使用（Ubuntu 26.04、Fedora、
  Arch）；在较旧的版本上请从源码构建（参见 [docs/zh-CN/install.md](docs/zh-CN/install.md#criu)）。
- GPU 作业还需要：其 `cuda-checkpoint` 支持 `--action lock/checkpoint/
  restore/unlock` 的 NVIDIA 驱动（r570 或更新版本；已测试：595）、`cuda-checkpoint` 二进制文件，以及 `nvidia-smi`。

详细信息参见 [docs/zh-CN/install.md](docs/zh-CN/install.md)。

<a id="limitations"></a>
## 限制

- **网络连接：** 为作业创建检查点时，已建立的 TCP 连接（连接到数据集服务器、数据库、实验跟踪服务等）
  会被关闭（`criu --tcp-close`）。恢复后程序会看到连接已断开，必须重新连接。
- **私有的 `/tmp` 和 `/dev/shm`：** 每个作业在 `/tmp` 和 `/dev/shm` 上都有自己的空 tmpfs。
  它会保存在检查点中，不与其他作业或你的 shell 共享。在重启后不会保留的挂载（`/run` 和 `/media` 下的任何内容）
  在作业内部不可见。请使用持久化文件系统上的路径来交换文件。
- **挂载在启动时固定：** 作业看到的是它启动时已挂载的文件系统（自动挂载点会在那时被触发），而不是之后才挂载的文件系统。
  要恢复作业，这些文件系统都必须重新挂载到相同的路径；否则恢复会被拒绝，镜像会被保留。
  `/scratch` 这类 tmpfs 上的文件在重启后不会保留。参见
  [文件系统与挂载](docs/zh-CN/install.md#filesystems-and-mounts)。
- **没有崩溃保护：** 只有在有序关机、`puzuz shutdown` 和 `puzuz checkpoint` 时才会创建检查点。
  断电、内核 panic 或硬重置时不会保存任何内容。暂不支持定期检查点。请继续保存你自己的训练检查点。
- **Fork guard：** CUDA 进程不经 `exec` 而 fork 出的子进程（例如使用 `fork` 启动方式的 PyTorch
  DataLoader worker）会继承 CRIU 无法保存的 NVIDIA 设备映射。helper 会向每个作业预加载一个小型库
  （`LD_PRELOAD`），在子进程中移除这些映射。它对静态链接的程序无效，对通过原始
  `clone(2)` 或 `_Fork()` 创建的进程也无效。启用该 guard 后，无法在这样 fork 出的子进程中初始化 NVML。
  PyTorch 会打印警告并继续运行。可以用 `fork_guard = false` 关闭它。
- **相同的 GPU 型号和驱动：** 作业只能在与创建检查点时相同型号的 GPU 上、使用相同的驱动版本并在相同的 GPU 编号上恢复。
  否则恢复会被拒绝，镜像会被保留。请在恢复（或删除）所有已创建检查点的作业之后再升级 NVIDIA 驱动。
- **相同的文件：** CRIU 按路径重新打开作业的文件，并检查可执行文件和库。在作业恢复之前，
  不要更改或删除作业的 virtualenv、二进制文件、工作目录或日志。不要把通往作业文件路径上的目录替换为符号链接，
  并保持你所属的组不变：恢复时会检查你能否以相同的方式、通过相同的目录打开作业曾打开的每个文件，否则恢复会被拒绝（参见
  [恢复与路径](docs/zh-CN/architecture.md#restores-and-paths)）。
- **Unix 套接字：** 如果作业的 Unix 套接字绑定在其自己的 `/tmp` 和
  `/dev/shm` 之外的路径上（例如你的主目录中），或绑定到 `ctl.sock` 这样的相对名称，就无法为它创建检查点；
  检查点会被拒绝，作业继续运行。请把套接字绑定到 `/tmp` 下的绝对路径，或使用抽象名称。
- **`SO_REUSEPORT`：** 如果作业有设置了 `SO_REUSEPORT` 的 TCP 或 UDP 套接字，同样无法为它创建检查点
  （CRIU 会把该套接字恢复为属于 root 的套接字，它随后可能与某个 root 服务共享端口）。有些服务器默认会设置它；
  请将其关闭，或使用 `--no-checkpoint`。
- **Python 和 PyTorch：**
  - Python 3.14 及更新版本在 Linux 上默认使用 `forkserver` 启动方式，因此 DataLoader worker
    不是 fork 出来的，也不依赖 fork guard。Python 3.12 和 3.13（或显式指定
    `multiprocessing_context="fork"`）则依赖 fork guard。
  - 在当前的驱动下，无法为进程间共享 CUDA 张量（`torch.multiprocessing` CUDA IPC）的作业创建检查点。
    使用 NCCL 的多 GPU 作业尚未测试。
  - cuda-checkpoint 会等待已提交的 GPU 工作完成后再保存，因此运行时间很长的 kernel 会推迟检查点。
  - 每个作业的检查点大约占用 RSS + 显存大小，再加上作业保存在其 `/tmp` 和 `/dev/shm` 中的内容的磁盘空间，
    并且一次写入一个作业。
    在测试中，一个占用 7 GB 显存的作业转储耗时约 10 到 20 秒。
- **覆盖 `/` 的挂载：** 如果作业的根目录被另一个挂载覆盖，就无法为它创建检查点。
  puzuz 会拒绝转储并告诉你原因。作业继续运行。
- 只要作业还存在，检查点镜像就不会被自动删除。在作业成功恢复、被删除（`remove`、`clean`、`reset`）
  或其转储失败时，镜像会被删除。超过 `checkpoint_size_limit_bytes` 时，puzuz 会发出警告，但不会拒绝。
- 暂不支持：`edit`、`env`、定期检查点、在保持作业运行的同时创建检查点、不做完整检查点而释放 GPU
  （"GPU suspend"）、Web UI 或 TCP API，以及按显存计数的槽位。

<a id="project-status"></a>
## 项目状态

预发布（版本 0.1.0）。目前已测试：

- 每个 crate 的单元测试和集成测试（`scripts/test-in-docker.sh`）。
- 在特权 Docker 容器中进行的全栈端到端测试：systemd 259 作为 PID 1、CRIU 4.2，
  二进制文件通过 `puzuz service install` 安装（`scripts/e2e-test.sh`）。测试涵盖基本的队列操作、
  三个用户的授权、组和 GPU 限制、检查点/恢复、守护进程和 helper 的重启、一次性添加大量作业、
  `puzuz shutdown`，以及模拟重启：用 systemd 关闭容器，并在新容器中恢复作业，其中也包括在独立的 ext4 文件系统、
  bind 挂载、`x-systemd.automount`、overlayfs、tmpfs 和 sshfs 上工作的作业，这些挂载会作为新的挂载重新出现。
- 在 Docker 中使用真实 GPU（驱动 595、手动模式的 CRIU 4.2.1、Python 3.12 上的 PyTorch 2.8）：
  CUDA 计数器以及使用 fork 出的 DataLoader worker 的 PyTorch 训练的检查点/恢复，
  其结果与未中断的运行完全相同；以及守护进程在打包好的、沙箱化的 `puzuz.service` 中通过 NVML 检测 GPU。
- QEMU/KVM 虚拟机的真实操作系统重启和关机（`scripts/vm-reboot-test.sh`）：Ubuntu 26.04、systemd 259 和
  CRIU 4.2，puzuz 通过 `puzuz service install` 安装，`/var/lib/puzuz` 位于独立的 ext4 磁盘上，运行两个用户的
  CPU 作业以及一个 `--no-checkpoint` 作业。场景包括 `systemctl reboot`、`systemctl poweroff` 后重新启动，
  以及在一个 1 GiB 作业的 `puzuz checkpoint` 仍在进行时重启。每次启动后，上一次启动的 journal 必须显示正确的
  停止顺序（守护进程最先停止，helper 在自身停止之前转储作业，`--no-checkpoint` 作业的 unit 只在 helper 之后
  停止，最后卸载 `/var/lib/puzuz`），且没有超时或
  SIGKILL；`--no-checkpoint` 作业必须收到了 SIGTERM；恢复后的作业必须从停止处继续计数。

尚未测试：

- 物理机（裸机）上的真实重启，以及真实重启中与 GPU 相关的部分：GPU 作业跨重启的检查点与恢复、驱动重新加载，
  以及相对于 `nvidia-persistenced` 的停止顺序。
- 从真实的 GitHub Release 用 `install.sh` 安装（已在 Ubuntu 22.04/24.04/26.04 和 Debian 12 容器中针对伪造的发布服务器测试，
  `scripts/dist-test.sh`）。
- 在运行着 systemd 的环境中使用 `.deb`（其安装、重新安装、卸载和清除已在不带 systemd 的普通 Debian 12 和 Ubuntu 容器中测试，
  `scripts/deb-test.sh`；通过 APT 仓库的本地副本进行升级也已测试，`scripts/apt-repo-test.sh`），`.rpm`
  同样如此（安装、升级和卸载已在普通的 Rocky Linux 9 和 Fedora 容器中测试，
  `scripts/rpm-test.sh`），真实 `apt` 发布版本中的 APT 仓库，以及真实硬件上的 arm64。
- 内核 NFS 和 autofs 间接映射（FUSE 网络文件系统和 `x-systemd.automount` 已测试）。
- 多 GPU 和 NCCL 作业，以及测试所用型号以外的 GPU 型号。

请报告在你的硬件上哪些功能可用、哪些不可用。

<a id="agent-skill"></a>
## 面向编码智能体的 Agent Skill

本仓库在 [skills/puzuz](skills/puzuz/SKILL.md) 中提供一个 [Agent Skill](https://agentskills.io)，
教编码智能体（Claude Code、Codex、Cursor 等）以普通用户身份使用 puzuz：检查守护进程是否在运行、
以正确的工作目录、环境变量和 `--gpu` 添加作业、跟踪日志、用 `puzuz wait` 等待而不是轮询、
pause/kill/restart、checkpoint/restore、`--json` 输出、常见错误，以及安全规则（只操作自己的作业，
不使用 `sudo` 或管理员命令）。使用 [skills](https://github.com/vercel-labs/skills) CLI 安装：

```sh
npx skills add kaito4681/puzuz              # 仅当前项目；加 -g 则对所有项目生效
npx skills add kaito4681/puzuz -g -a claude-code -y
```

<a id="documentation"></a>
## 文档

- [docs/zh-CN/install.md](docs/zh-CN/install.md)：要求、构建、安装、配置、
  优先级与公平共享、升级、卸载和故障排查
- [docs/zh-CN/architecture.md](docs/zh-CN/architecture.md)：组件、作业流程、关机顺序、状态存储和
  crate 布局

<a id="contributing"></a>
## 参与贡献

工作区布局和约定参见 [AGENTS.md](AGENTS.md)。代码、注释、文档和提交信息均使用英文。测试在 Docker 中运行：

```sh
scripts/test-in-docker.sh                                   # cargo test --workspace
scripts/test-in-docker.sh clippy --workspace --all-targets
scripts/test-in-docker.sh fmt --all
scripts/e2e-test.sh               # full stack under systemd in a privileged container
scripts/vm-reboot-test.sh         # real reboot/poweroff of a QEMU/KVM VM with CPU jobs (needs /dev/kvm)
scripts/service-install-test.sh   # service install / status / uninstall and doctor
scripts/helper-systemd-test.sh    # the helper alone under systemd
scripts/dist-test.sh              # install.sh against a fake GitHub API (PUZUZ_DIST_BASE=debian:12 ...)
nix build -L .#checks.x86_64-linux.nixos         # NixOS module in a VM (needs KVM)
nix build -L .#checks.x86_64-linux.home-manager  # home-manager module evaluation
nix build -L .#checks.x86_64-linux.helper-tests  # all puzuz-helper tests in a VM (needs KVM)
```

`scripts/e2e-test.sh` 在文件开头说明了它的环境变量，例如
`PUZUZ_E2E_ONLY`，以及用于 GPU 场景的 `PUZUZ_E2E_GPU=1`。`PUZUZ_HELPER_GPU=1
scripts/helper-systemd-test.sh`（需要 CUDA 基础镜像，参见该脚本）会强制让 GPU 检查点超过其期限，并在转储中途终止
`criu` / `cuda-checkpoint`，两种 `PUZUZ_HELPER_CUDA_MODE` 均会测试。`scripts/measure-checkpoint.sh`
测量每个检查点步骤的耗时。这些脚本都不会在主机上执行任何特权操作。

<a id="license"></a>
## 许可证

[MIT](LICENSE)
