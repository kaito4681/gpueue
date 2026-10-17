<a id="gpueue"></a>
# gpueue

[![CI](https://github.com/kaito4681/gpueue/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/kaito4681/gpueue/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/kaito4681/gpueue)](https://github.com/kaito4681/gpueue/releases/latest)
[![License: MIT](https://img.shields.io/github/license/kaito4681/gpueue)](LICENSE)

[English](README.md) | [日本語](README.ja.md) | 简体中文

面向多用户共享的单台 Linux 机器的 GPU 作业队列。其 CLI 的用法与
[pueue](https://github.com/Nukesor/pueue) 相同。gpueue 在此基础上增加了两项功能：GPU 槽位，以及借助
[CRIU](https://criu.org) 和 NVIDIA [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint)
对运行中的作业（CPU、内存和 GPU 状态）进行透明的检查点保存与恢复。机器重启时仍在运行的训练会被保存到磁盘，
并从中断处继续运行，而不必从头开始。

> [!NOTE]
> **状态：预发布。** 下文所述的全部功能均已实现，并已在 Docker 中测试（systemd 作为 PID 1、
> 真实的 CRIU、真实的 NVIDIA GPU），并在运行 CPU 作业的 QEMU/KVM 虚拟机的真实重启和关机中测试过。
> 但尚**未**在物理机（裸机）上或带着 GPU 作业经历真实重启的测试。
> 参见[项目状态](#project-status)。

<a id="contents"></a>
## 目录

- [为什么需要 gpueue](#why)
- [功能](#features)
- [安装](#install)
  - [1. 前提条件](#1-prerequisites)
  - [2. 安装 gpueue](#2-install-gpueue)
  - [3. 检查安装](#3-check-the-installation)
- [用法](#usage)
- [限制](#limitations)
- [项目状态](#project-status)
- [面向编码智能体的 Agent Skill](#agent-skill)
- [文档](#documentation)
- [参与贡献](#contributing)
- [许可证](#license)

<a id="why"></a>
## 为什么需要 gpueue

在共享的 GPU 工作站上，人们通常用 `nohup`、`tmux` 或每个用户各自的 pueue 来运行作业，并在聊天群里争抢 GPU。
为了更新内核或驱动而重启，会终止所有正在进行的训练。gpueue 让所有人共用一个队列：一个系统级守护进程调度所有用户的
作业，并显示谁在使用哪块 GPU。作业申请指定的 GPU（`--gpu 0,1`），并且只在这些 GPU 空闲时才启动。在操作系统关机时、
执行 `gpueue shutdown` 时，或通过 `gpueue checkpoint` 按需保存运行中的作业，之后用 `gpueue restore` 继续运行，
无需修改你的代码。

<a id="features"></a>
## 功能

| 功能 | 说明 |
| --- | --- |
| 兼容 pueue 的 CLI | `add`、`status`、`log`、`follow`、`wait`、`pause`、`start`、`kill`、`remove`、`restart`、`clean`、`reset`、`stash`、`enqueue`、`switch`、`group`、`parallel`、`shutdown`、`completions`，以及组、依赖（`--after`）、优先级、延迟和标签 |
| GPU 槽位 | 按 GPU 限制同时运行的作业数（`gpueue gpu parallel`）。每个作业会获得 `CUDA_VISIBLE_DEVICES` 和 `CUDA_DEVICE_ORDER=PCI_BUS_ID`。GPU 通过 NVML 检测 |
| 公平共享 | Slurm 风格的多因子优先级（带衰减用量的 fair share、等待时间、nice、`top`）；`gpueue prio` 和 `gpueue share` 显示具体数值 |
| 检查点/恢复 | 对 CPU 和 CUDA 进程进行透明的检查点保存与恢复，包括 DataLoader worker 通过 fork 创建的 PyTorch 作业（参见 [fork guard](docs/zh-CN/usage.md#limitations)）；操作系统关机或重启时，自动为所有运行中的作业创建检查点 |
| 多用户 | 作业以其所有者的身份运行。其他用户可以看到作业的存在，但看不到其命令、环境变量或日志。管理员是 root 和 `gpueue-admin` 组的成员 |
| 权限分离 | 守护进程（HTTP API、调度器、SQLite）以非特权系统用户 `gpueue` 运行。只有一个小型的 root helper 负责启动作业和运行 CRIU |
| 不中断的作业 | 作业运行在各自的 systemd 单元中，因此守护进程或 helper 重启时（例如升级期间）作业会继续运行 |
| `gpueue doctor` | 检查安装情况，并针对每个问题打印修复命令 |

<a id="install"></a>
## 安装

gpueue 由管理员在每台机器上安装一次；普通用户无需安装任何东西。每一步的详细说明参见
[docs/zh-CN/install.md](docs/zh-CN/install.md)。

<a id="1-prerequisites"></a>
### 1. 前提条件

gpueue 需要带有 systemd 240 或更新版本（已测试：259）的 Linux，以及启用了 `CONFIG_CHECKPOINT_RESTORE` 的内核
（推荐 5.5 或更新版本，已测试：7.0）。发布版本针对 x86_64 和 aarch64 构建，需要 glibc 2.34 或更新版本
（Ubuntu 22.04+、Debian 12+、RHEL 9+）；只有 x86_64 在真实硬件上测试过。

没有下面这些程序作业也能运行，但创建检查点需要它们，而任何安装方式都不会替你安装：软件包只是推荐 CRIU，而在许多
发行版版本上它过旧或不存在；`install.sh` 只会列出缺少的内容
（参见[各安装方式会安装什么](docs/zh-CN/install.md#dependencies-that-are-not-installed-automatically)）。
NixOS 模块自带 CRIU。

<a id="dependencies-that-are-not-installed-automatically"></a>

| 依赖 | 用途 | 获取方式 |
| --- | --- | --- |
| CRIU 4.2 或更新版本 | 所有检查点 | `sudo apt install criu`（Ubuntu 26.04）、`sudo dnf install criu`（Fedora）、`sudo pacman -S criu`（Arch Linux）；其他系统请[从源码构建](docs/zh-CN/install.md#criu) |
| NVIDIA 驱动 r570 或更新版本，含 `nvidia-smi`（已测试：595） | GPU 作业 | 来自发行版；其 `cuda-checkpoint` 必须支持 `--action lock/checkpoint/restore/unlock` |
| 与驱动主版本相同的 NVIDIA 预编译 cuda-checkpoint，属主为 root | GPU 检查点 | 下面的命令（NixOS 上参见 [NixOS 上的 cuda-checkpoint](docs/zh-CN/install.md#cuda-checkpoint-on-nixos)） |

```sh
git clone --depth 1 https://github.com/NVIDIA/cuda-checkpoint /tmp/cuda-checkpoint
sudo install -o root -g root -m 0755 "/tmp/cuda-checkpoint/bin/$(uname -m)_Linux/cuda-checkpoint" \
  /usr/local/bin/cuda-checkpoint
```

helper 只在启动时查找 cuda-checkpoint 和 CRIU：如果在 gpueue 之后才安装其中任一个，请用
`sudo systemctl restart gpueue-helper` 重启 helper（参见 [cuda-checkpoint](docs/zh-CN/install.md#cuda-checkpoint)），
然后再次运行 `gpueue doctor`。

<a id="2-install-gpueue"></a>
### 2. 安装 gpueue

安装脚本会下载最新的发布版本、进行校验，将 gpueue 安装到 /usr/local/bin，设置 systemd 单元、`gpueue` 用户、
`gpueue-admin` 组、各个目录以及默认的 /etc/gpueue/config.toml，启动服务，并打印 `gpueue doctor` 的报告：

```sh
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

之后用 `sudo gpueue update` 更新（运行中的作业会继续运行）。用 `sudo gpueue uninstall` 卸载，数据会被保留
（加上 `--purge` 则一并删除）。不需要 GitHub token（如果设置了 `GH_TOKEN` 则会使用它；它只用于提高 API 速率限制）。

<details>
<summary><b>其他方式：APT / .deb、.rpm、NixOS 与 Nix、Arch Linux</b></summary>

在 Debian 和 Ubuntu（amd64 或 arm64）上，也可以改为从 gpueue 的 APT 仓库（位于 GitHub Release `apt` 中）
安装软件包；这需要 `curl` 和 `ca-certificates`，最新的稳定版本和架构由 apt 自动选择：

```sh
sudo curl -fsSLo /etc/apt/keyrings/gpueue.gpg https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/gpueue.sources https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.sources
sudo apt update && sudo apt install gpueue
```

或者直接安装最新发布版本的 `.deb`（它会注册同一个仓库）：

```sh
arch=$(dpkg --print-architecture)
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb"
sudo apt install "./gpueue_${arch}.deb"
```

它会安装并启动相同的服务，并推荐安装 `criu`（前提条件参见
[docs/zh-CN/install.md](docs/zh-CN/install.md#installing-the-debianubuntu-package)）。无论哪种方式，之后的版本
都通过 `sudo apt update && sudo apt upgrade` 获取。`sudo apt remove gpueue` 会停止服务；`sudo apt purge gpueue`
还会删除 /etc/gpueue。两者都会保留 /var/lib/gpueue 和运行中的作业。

在 Fedora、RHEL、Rocky 和 Alma Linux 9+（x86_64 或 aarch64）上，安装发布版本中的 .rpm
（dnf 可以直接接受 URL）：

```sh
sudo dnf install "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue.$(uname -m).rpm"
```

这些 URL 始终指向最新的稳定发布版本（`releases/latest` 会跳过预发布版本）；带版本号的文件
（`gpueue_<version>_<arch>.deb`、`gpueue-<version>-1.<arch>.rpm`）也一并附带，用于安装特定版本。
每个文件都有 `.sha256` 和 `.minisig` 文件（参见
[手动验证发布版本](docs/zh-CN/install.md#verifying-a-release-by-hand)）。

目前还没有 dnf 仓库（已在计划中，issue #23）：更新时用 `sudo dnf upgrade` 运行同一条命令。`sudo dnf remove gpueue` 会停止服务，并保留 /var/lib/gpueue 和运行中的作业。它推荐安装 `criu`；
Fedora 的版本足够新，而 Rocky/RHEL 9 只提供 3.19，因此在那里创建检查点需要更新的 CRIU（没有它作业也能运行）。
你也可以从源码构建，或使用 Nix flake（参见 [docs/zh-CN/install.md](docs/zh-CN/install.md)）。

在 NixOS 上，导入 flake 的模块（`github:kaito4681/gpueue/stable`、`nixosModules.default`）并设置
`services.gpueue.enable = true`（参见 [NixOS](docs/zh-CN/install.md#nixos)）；home-manager 用户可以通过
`homeManagerModules.default` 添加 CLI 的补全。在其他发行版上使用 Nix（x86_64 或 aarch64）时，构建该 flake，
然后从构建结果安装服务（它会被复制到 /usr/local/bin，并且不会被 Nix 的垃圾回收清除；参见
[使用 Nix 安装](docs/zh-CN/install.md#installing-with-nix)）：

```sh
nix build github:kaito4681/gpueue/stable
sudo ./result/bin/gpueue service install
```

在 Arch Linux（x86_64、aarch64）上，目前请使用上面的安装脚本：AUR 软件包 `gpueue-bin` 计划发布但尚未发布（[#159](https://github.com/kaito4681/gpueue/issues/159)）。
CRIU（4.2 或更新版本，用于检查点）位于官方仓库中：

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

用 `sudo gpueue update` 更新，用 `sudo gpueue uninstall` 卸载。软件包发布到 AUR 后，你也可以改用任意 AUR
helper 安装 [`gpueue-bin`](https://aur.archlinux.org/packages/gpueue-bin)（参见
[docs/zh-CN/install.md](docs/zh-CN/install.md#arch-linux-aur)）。

</details>

<a id="3-check-the-installation"></a>
### 3. 检查安装

```sh
# Check everything: criu, cuda-checkpoint, driver, kernel, systemd, services, storage.
gpueue doctor

# Optional: make someone an administrator.
sudo usermod -aG gpueue-admin alice
```

<a id="usage"></a>
## 用法

`gpueue --help` 列出所有命令。没有 `(gpueue)` 标记的命令与 pueue 的用法相同（[差异](docs/zh-CN/usage.md#pueue-compatibility-and-differences)
主要在于共用一个守护进程以及谁可以做什么）；带标记的是 gpueue 新增的命令。`gpueue <command> --help` 显示每个命令的
选项，例如 `add` 的 `--gpu` 和 `--no-checkpoint`。

```console
$ gpueue --help
Interact with the gpueue GPU job queue (a pueue-compatible CLI)

Use the `--help` long form to get detailed help output on each subcommand!

Usage: gpueue [OPTIONS] [COMMAND]

Commands:
  add          Enqueue a job for execution
  remove       Remove jobs from the list
  switch       Swap the queue position of two jobs
  stash        Stash jobs so they won't be started automatically
  enqueue      Enqueue stashed jobs again
  start        Resume paused jobs or groups, or force-start queued jobs
  restart      Restart finished jobs
  pause        Pause running jobs or whole groups
  kill         Kill running jobs or all jobs of a group
  send         Not supported by gpueue: jobs have no stdin to send input to
  edit         Edit properties of stashed or queued jobs in $EDITOR (not implemented yet)
  env          Add or remove environment variables of a job (not implemented yet)
  group        Add or remove groups; without a subcommand, list all groups
  status       Show the status of all jobs
  log          Show the output of jobs
  follow       Follow the output of a running job, like "tail -f"
  wait         Wait until jobs are finished
  clean        Remove all finished jobs from the list
  reset        Kill all jobs, remove them and reset the groups
  shutdown     Checkpoint all running jobs and stop the daemon (administrators only)
  parallel     Set the number of jobs a group may run in parallel
  completions  Generate shell completion files
  checkpoint   (gpueue) Checkpoint running jobs and stop them
  restore      (gpueue) Resume Checkpointed jobs
  gpu          (gpueue) Show the GPUs and their slots; see subcommands to change limits
  prio         (gpueue) Show the priority of pending jobs and its factors (like Slurm's `sprio`)
  nice         (gpueue) Set the nice value of jobs (like Slurm's nice option): it is subtracted from
               their priority
  top          (gpueue) Move jobs to the top of their owner's queue (Slurm's `scontrol top`)
  site         (gpueue) Set the site factor of jobs, which is added to their priority
               (administrators only)
  share        (gpueue) Show the fair-share state of each user (like Slurm's `sshare`)
  daemon       (gpueue) Run the daemon in the foreground (normally started by systemd)
  service      (gpueue) Install, uninstall or inspect the systemd services
  uninstall    (gpueue) Uninstall gpueue (requires root)
  doctor       (gpueue) Check the installation and print how to fix problems
  update       (gpueue) Update gpueue itself (root; without root it only checks)
  help         Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose...
          Verbose mode (-v, -vv, -vvv)

      --color <COLOR>
          Colorize the output; `auto` enables colors when stdout is a terminal
          
          [default: auto]
          [possible values: auto, never, always]

      --socket <SOCKET>
          Path of the daemon's Unix socket.
          
          Clients connect to it and `gpueue daemon` listens on it. Replaces pueue's
          `--config`/`--profile`, which do not apply to a system-wide daemon.
          
          [env: GPUEUE_SOCKET=]
          [default: /run/gpueue/gpueue.sock]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

典型的用法：`add` 在指定的 GPU 上将作业排队，`status` 和 `gpu` 显示谁在哪里运行什么，`log` 和 `follow` 显示其输出，
`checkpoint` / `restore` 把作业从 GPU 上撤下并在之后恢复。

```sh
# Queue a job on GPU 0. The command runs through `sh -c` in the current directory, with your
# environment.
gpueue add --gpu 0 -l resnet -- 'python train.py --epochs 100'

gpueue status          # all jobs of all users (others' commands are hidden)
gpueue gpu             # GPUs, slots in use, and which jobs hold them
gpueue log 0           # the last lines of job 0's output
gpueue follow 0        # stream the output, like tail -f

gpueue checkpoint 0    # save job 0 to disk and stop it; frees the GPU. State: Checkpointed
gpueue restore 0       # queue it again; it resumes once its group and GPUs have a free slot
gpueue restore -i 0    # ...or put it first among your queued jobs (administrators: resume right away; see "Job priority")
```

恢复后的作业从创建检查点的那一刻继续运行，保留原来的作业 id，其输出会追加到同一个日志中。操作系统关机时，helper
逐个为运行中的作业创建检查点（`--no-checkpoint` 作业则直接被终止）；启动后它们显示为 `Checkpointed`，每个所有者用
`gpueue restore <id>` 或 `gpueue restore -a` 恢复它们：恢复不会自动开始。只有管理员能用 `start`、`add -i`、
`restart -i` 或 `restore -i` 立即启动作业；其他用户的作业会被排到其自己作业的最前面。

其余内容参见参考文档 [docs/zh-CN/usage.md](docs/zh-CN/usage.md)：
[与 pueue 的差异和额外的命令](docs/zh-CN/usage.md#pueue-compatibility-and-differences)、
[JSON 输出](docs/zh-CN/usage.md#json-output)、
[暂停的作业、重启和失败的检查点](docs/zh-CN/usage.md#checkpoint-and-restore)、
[谁可以做什么](docs/zh-CN/usage.md#multi-user-model-and-authorization)、
[GPU 槽位与隔离](docs/zh-CN/usage.md#gpu-slots)以及
[作业优先级](docs/zh-CN/usage.md#job-priority)。

<a id="limitations"></a>
## 限制

检查点/恢复有一些限制。创建检查点时已建立的 TCP 连接会被关闭，断电或内核崩溃时不会保存任何内容。作业只能在相同的
GPU 型号和驱动上、在相同的文件和挂载下恢复。持有某些套接字或描述符（作业的 `/tmp` 之外的 Unix 套接字、inotify、
`SO_REUSEPORT` 等）的作业无法创建检查点；此时检查点会被拒绝，作业继续运行。参见[全部限制](docs/zh-CN/usage.md#limitations)。

<a id="project-status"></a>
## 项目状态

预发布（版本 0.1.4）。v0.1.4 是一个安全发布版本：运行 v0.1.3 或更早版本的站点应当升级（参见
[v0.1.4 的发布说明](https://github.com/kaito4681/gpueue/releases/tag/v0.1.4)）。测试包括单元测试和集成测试、在 Docker
中 systemd 下的全栈端到端测试（也使用真实 GPU），以及 QEMU/KVM 虚拟机的真实重启；物理机上的真实重启以及真实重启中
与 GPU 相关的部分尚未测试（参见[测试范围](docs/zh-CN/usage.md#test-coverage)）。请报告在你的硬件上哪些功能可用、哪些不可用。

<details>
<summary><b>升级前需要确认的 v0.1.4 行为变更</b></summary>

- 以下作业不能再创建检查点（拒绝消息会说明原因）：持有 inotify/fanotify 监视的作业、在抽象 Unix 套接字上监听的作业
  （libcuda 自己的除外）、在 Unix 套接字队列中持有描述符的作业，以及持有已删除的 setuid/setgid 文件的作业。
- 当主机上的其他程序占用了作业曾绑定的端口时，恢复会被拒绝；端口空闲后可以重试。
- 如果 `/etc/gpueue/config.toml`（或其上层目录）不属于 root，或可被组或其他用户写入，`gpueue daemon` 会拒绝启动；
  `gpueue doctor` 会报告这一点。
- `sudo gpueue service install` 拒绝从非 root 用户可写的目录复制 `gpueue-helper`；请以 root 身份解压发布包。
- v0.1.3 创建的检查点可以用 v0.1.4 恢复。

</details>

<a id="agent-skill"></a>
## 面向编码智能体的 Agent Skill

本仓库在 [skills/gpueue](skills/gpueue/SKILL.md) 中提供一个 [Agent Skill](https://agentskills.io)，
教编码智能体（Claude Code、Codex、Cursor 等）以普通用户身份使用 gpueue：检查守护进程是否在运行、
以正确的工作目录、环境变量和 `--gpu` 添加作业、跟踪日志、用 `gpueue wait` 等待而不是轮询、
pause/kill/restart、checkpoint/restore、`--json` 输出、常见错误，以及安全规则（只操作自己的作业，
不使用 `sudo` 或管理员命令）。使用 [skills](https://github.com/vercel-labs/skills) CLI 安装：

```sh
npx skills add kaito4681/gpueue              # 仅当前项目；加 -g 则对所有项目生效
npx skills add kaito4681/gpueue -g -a claude-code -y
```

<a id="documentation"></a>
## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/zh-CN/install.md](docs/zh-CN/install.md) | 要求、构建、安装、配置、优先级与公平共享、升级、卸载和故障排查 |
| [docs/zh-CN/usage.md](docs/zh-CN/usage.md) | 与 pueue 的差异、JSON 输出、检查点/恢复、多用户模型、GPU 槽位、作业优先级、限制和测试范围 |
| [docs/zh-CN/architecture.md](docs/zh-CN/architecture.md) | 组件、作业流程、关机顺序、状态存储和 crate 布局 |

<a id="contributing"></a>
## 参与贡献

工作区布局和约定参见 [AGENTS.md](AGENTS.md)。代码、注释、文档和提交信息均使用英文。测试在 Docker 中运行：

```sh
scripts/test-in-docker.sh                                   # cargo test --workspace
scripts/test-in-docker.sh clippy --workspace --all-targets
scripts/test-in-docker.sh fmt --all
```

<details>
<summary><b>端到端、虚拟机、GPU 和 Nix 测试</b></summary>

```sh
scripts/e2e-test.sh               # full stack under systemd in a privileged container
scripts/vm-reboot-test.sh         # real OS in QEMU/KVM VMs: install, reboots, upgrade (needs /dev/kvm; `all` for every distro)
scripts/gpu-host-test.sh          # checkpoint/restore of real CUDA jobs on the installed gpueue (GPU host, no root)
scripts/service-install-test.sh   # service install / status / uninstall and doctor
scripts/helper-systemd-test.sh    # the helper alone under systemd
scripts/dist-test.sh              # install.sh against a fake GitHub API (GPUEUE_DIST_BASE=debian:12 ...)
nix build -L .#checks.x86_64-linux.nixos         # NixOS module in a VM (needs KVM)
nix build -L .#checks.x86_64-linux.home-manager  # home-manager module evaluation
nix build -L .#checks.x86_64-linux.helper-tests  # all gpueue-helper tests in a VM (needs KVM)
```

`scripts/e2e-test.sh` 在文件开头说明了它的环境变量，例如
`GPUEUE_E2E_ONLY`，以及用于 GPU 场景的 `GPUEUE_E2E_GPU=1`。`GPUEUE_HELPER_GPU=1
scripts/helper-systemd-test.sh`（需要 CUDA 基础镜像，参见该脚本）会强制让 GPU 检查点超过其期限，并在转储中途终止
`criu` / `cuda-checkpoint`，两种 `GPUEUE_HELPER_CUDA_MODE` 均会测试。`scripts/measure-checkpoint.sh`
测量每个检查点步骤的耗时。这些脚本都不会在主机上执行任何特权操作。

</details>

<a id="license"></a>
## 许可证

[MIT](LICENSE)
