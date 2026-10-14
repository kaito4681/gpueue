<a id="installing-and-operating-puzuz"></a>
# puzuz 的安装与运维

[English](../install.md) | [日本語](../ja/install.md) | 简体中文

本指南面向机器的管理员。普通用户只需在其 `PATH` 中有 `puzuz` 命令即可，无需其他任何东西。

- [系统要求](#requirements)
- [使用 install.sh 安装](#installing-with-installsh)
- [安装 Debian/Ubuntu 软件包](#installing-the-debianubuntu-package)
- [安装 Fedora/RHEL/Rocky 软件包](#installing-the-fedorarhelrocky-package)
- [使用 Nix 安装](#installing-with-nix)
- [Arch Linux (AUR)](#arch-linux-aur)
- [构建](#building)
- [安装](#installing)
- [验证](#verifying)
- [配置](#configuration)
- [管理员](#administrators)
- [谁可以运行作业](#who-jobs-may-run-as)
- [作业能做什么、不能做什么](#what-a-job-can-and-cannot-do)
- [优先级与公平共享](#priority-and-fair-share)
- [GPU 检测](#gpu-detection)
- [GPU 隔离](#gpu-isolation)
- [检查点存储](#checkpoint-storage)
- [升级](#upgrading)
- [卸载](#uninstalling)
- [故障排查](#troubleshooting)

<a id="requirements"></a>
## 系统要求

| 组件 | 要求 | 测试环境 |
| --- | --- | --- |
| 操作系统 | 以 systemd 作为 init 的 Linux，x86_64 或 aarch64（发布版本为两者都构建了二进制） | Ubuntu（以 systemd 作为 PID 1 的 Docker 镜像），x86_64 |
| systemd | 240 或更高版本（由 `puzuz doctor` 检查） | 259 |
| 内核 | `CONFIG_CHECKPOINT_RESTORE=y`（必须存在 `/proc/sys/kernel/ns_last_pid`）、PID 命名空间和挂载命名空间；推荐 5.5 或更高版本（CRIU 用带 `set_tid` 的 `clone3` 来恢复 PID）；`open_tree`/`move_mount` 需要 5.2 或更高版本（用于清理被隐藏的挂载）；恢复需要 5.8 或更高版本（`openat2`、`faccessat2` 以及 `statx` 的挂载 ID，用于检查恢复时打开了什么） | 7.0 |
| CRIU | 4.2 或更高版本 | 4.2（发行版软件包）、4.2.1 |
| NVIDIA 驱动（GPU 作业） | 其 `cuda-checkpoint` 支持 `--action lock/checkpoint/restore/unlock` 和 `--timeout` 的驱动：r570 或更高版本（cuda-checkpoint 本身支持 r550，但仅支持 `--toggle`，而 puzuz 不使用它） | 595 |
| cuda-checkpoint（GPU 作业） | NVIDIA 提供的二进制文件，主版本号须与驱动相同 | 595 |
| nvidia-smi（GPU 作业） | helper 用它查找 CUDA 进程和 GPU 显存 | 随驱动提供 |
| Rust（仅构建时） | 1.88 或更高版本，另需一个 C 编译器（`cc`）用于构建 helper 的 fork guard 库 | 1.98.1 |

没有 NVIDIA 驱动时，puzuz 仍然可以运行仅使用 CPU 的作业，并同样可以为它们创建检查点。

<a id="criu"></a>
### CRIU

puzuz 需要 CRIU 4.2 或更高版本才能创建检查点；没有它作业也能运行。`puzuz doctor` 会报告 CRIU
缺失或版本过旧。在 Ubuntu 26.04（`sudo apt install criu`，universe）、Fedora（`sudo dnf install criu`，4.2）
和 Arch（`sudo pacman -S criu`）上，发行版的软件包版本足够新。Ubuntu 22.04（3.16）、Ubuntu 24.04（无软件包）、
Debian 12（3.17）、Debian 13（4.1）以及 RHEL/Rocky/Alma 9（3.19）则不满足：请按照
<https://criu.org/Installation> 从源码构建 CRIU（使用上游发布的源码包，执行 `make` 和 `sudo make install`
安装到 `/usr/local/sbin`），或者将 `/etc/puzuz/config.toml` 中的 `criu_path` 指向一个属于 root 的构建产物。
`.deb` 和 `.rpm` 只是推荐（recommends）`criu`，因此在这些发行版上 apt 或 dnf 会安装旧版软件包或什么都不装；
`/usr/local/sbin` 中的 CRIU 在 helper 的 `PATH` 中优先。在默认的 **manual** 模式
（`cuda_checkpoint_mode = "manual"`）下，helper 自己调用 cuda-checkpoint，并以不加载插件的方式运行 CRIU。
发行版软件包可以满足这种用法，即使它不带 CRIU 的 CUDA 插件。

**plugin** 模式（`cuda_checkpoint_mode = "plugin"`）则改由 CRIU 的 CUDA 插件（`cuda_plugin.so`，
CRIU 4.0 或更高版本）驱动 cuda-checkpoint。该模式是可选的；参见
[CUDA 插件模式](#cuda-plugin-mode)。

<a id="cuda-checkpoint"></a>
### cuda-checkpoint

NVIDIA 在 <https://github.com/NVIDIA/cuda-checkpoint> 中以预编译二进制的形式分发 cuda-checkpoint，
位于 `bin/x86_64_Linux/cuda-checkpoint`（arm64 上为 `bin/aarch64_Linux/`）。请将它放在 root 的 `PATH`
能找到的位置，属主为 root，且其他用户不可写：

```sh
sudo install -o root -g root -m 0755 cuda-checkpoint /usr/local/bin/cuda-checkpoint
```

也可以放在其他位置，并在 `/etc/puzuz/config.toml` 中设置 `cuda_checkpoint_path`。`criu`、`cuda-checkpoint`
和 `nvidia-smi` 的绝对路径必须属于 root，因为 helper 以 root 身份运行它们。属于 root 的符号链接会被跟随，
其目标以同样方式检查；`/nix/store`（属于 root、设置了 sticky 位、组为 `nixbld`）虽然组可写，也会被接受，
因此 NixOS 上类似 `/run/current-system/sw/bin/criu` 的路径可以正常使用。以 root 身份运行的
`puzuz doctor` 在运行这些程序之前也应用同样的规则：不满足规则的程序不会被运行，而是报告为 FAIL
（不含路径的名称在 systemd 的默认 `PATH` 中查找）；如果配置文件本身未通过所有者检查，它指定的
程序一个也不会运行。

<a id="installing-with-installsh"></a>
## 使用 install.sh 安装

`scripts/install.sh` 是在 x86_64 和 aarch64 Linux 上安装 puzuz 的常规方式（在 Debian 和 Ubuntu 上，
也可以改用[软件包](#installing-the-debianubuntu-package)）。每个发布版本都附带该脚本。它下载发布版本的
tarball 并进行校验，然后运行该发布版本的 `puzuz service install`，将二进制文件复制到 `/usr/local/bin`
（其具体行为参见[安装](#installing)）。

<a id="github-token-optional"></a>
### GitHub 令牌（可选）

下载不需要 GitHub 令牌。令牌只是提高匿名请求的 GitHub API 速率限制（每个 IP 地址每小时 60 次，
共享 NAT 可能会用完）。任何令牌都可以；细粒度（fine-grained）令牌不需要任何权限。`install.sh`
按以下顺序查找令牌，找到即使用：

1. 环境变量 `GH_TOKEN` 或 `GITHUB_TOKEN`，
2. `--token-file FILE`（FILE 的第一行），
3. 以执行 `sudo` 的用户身份运行 `gh auth token`（如果已安装并登录了 GitHub CLI），
4. `/etc/puzuz/github-token`，由 `--save-token` 写入（属于 root，模式 0600）。

令牌通过头部文件传给 `curl`，因此不会出现在 `ps` 或输出中。被拒绝的令牌会以明确说明这一点的消息失败；
`--version X` 返回 404 表示该发布版本不存在。

<a id="running-the-installer"></a>
### 运行安装程序

```sh
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

脚本来自管道时，选项写在 `bash -s --` 之后（例如 `... | sudo bash -s -- --version 0.2.0`）；
使用下载好的副本时，选项写在文件名之后（`sudo bash install.sh --version 0.2.0`；如何在以 root 运行前校验其
签名，见[手动验证发布版本](#verifying-a-release-by-hand)）：

| 选项 | 含义 |
| --- | --- |
| `--version X` | 安装版本 X，而不是最新的发布版本 |
| `--bin-dir DIR` | 安装到默认位置以外的地方（默认是当前安装所在的目录，否则为 `/usr/local/bin`） |
| `--token-file FILE` | 从 FILE 读取令牌 |
| `--save-token` | 将令牌保存到 `/etc/puzuz/github-token`（root，0600），供 `puzuz update` 和以后的运行使用 |
| `--uninstall` | 使用已安装的 `puzuz uninstall` 移除 puzuz（参见[卸载](#uninstalling)） |
| `--purge` | 与 `--uninstall` 一起使用时，同时删除 `/etc/puzuz` 和 `/var/lib/puzuz` |
| `-y`, `--yes` | 清除（purge）前不再询问 |

只有指定 `--save-token` 时才会保存令牌。这样，过期或被吊销的令牌就不会在你不知道用了哪个令牌的情况下
导致之后的运行失败。

`install.sh` 在下载任何东西之前会检查：是否以 root 身份运行、systemd 是否为 PID 1、机器是否为 x86_64 或
aarch64 且 glibc 为 2.34 或更高版本，以及 puzuz 是否并非由包管理器或 Nix 安装（参见
[安装方式](#install-method)）。对于最后一项检查，它通过 `puzuz service install-method` 询问已安装的 puzuz
（单元所运行的那个，否则为 `PATH` 中的那个）；如果没有已安装的 puzuz，它自己只拒绝明显的情况
（通过 `/etc/os-release` 识别出的 NixOS、属于某个软件包的单元、链接到 `/nix/store` 的单元），
其余情况由下载的二进制文件在安装任何东西之前判断。之后它会：

1. 查找发布版本（`/repos/kaito4681/puzuz/releases/latest` 或 `/releases/tags/v<X>`），
2. 将 `puzuz-v<X>-<target>.tar.gz`（`x86_64-unknown-linux-gnu` 或 `aarch64-unknown-linux-gnu`）
   及其 `.sha256` 和 `.minisig` 文件下载到临时目录（只用 HTTPS，重定向也是如此），确认 `.sha256` 文件是按名称给出的该
   tarball 的校验和，并校验校验和，
3. 用内置于脚本中的发布密钥（`packaging/minisign.pub`）校验 `.minisig` 签名。它使用 `PATH` 中的
   `minisign`；如果没有，就从 GitHub 将 minisign 0.12 的静态二进制发布包下载到临时目录，用内置于脚本中的
   SHA-256 校验和（与 `scripts/dist/get-minisign.sh` 相同）校验后使用。如果下载失败，它会停止，并提示你
   从发行版安装 `minisign`（例如 `sudo apt install minisign`）后重新运行；签名校验绝不会被跳过，
4. 解压 tarball（拒绝链接以及指向其外部的路径），运行新的 `puzuz service install`
   （它沿用当前安装所在的目录，否则使用 `/usr/local/bin`；只有你指定了 `--bin-dir` 时才会传递该选项），
   并打印其 `puzuz doctor` 报告，
5. 如果缺少 CRIU 和 cuda-checkpoint，打印安装它们的方法。它从不自行安装它们。

如果只有部分 `puzuz doctor` 检查失败（例如缺少 CRIU）但服务正在运行，`install.sh` 会成功并给出警告。

之后，使用 `sudo puzuz update` 更新（参见[升级](#upgrading)）。再次运行 `install.sh` 也会将最新（或指定的）
版本覆盖安装到已安装的版本之上。无论哪种方式，正在运行的作业都会继续运行。

在 Debian 和 Ubuntu 上可以改用 [.deb](#installing-the-debianubuntu-package)，在 Fedora、RHEL、Rocky 和
Alma Linux 上可以改用 [.rpm](#installing-the-fedorarhelrocky-package)，在任何地方都可以使用
[Nix](#installing-with-nix)，在 Arch Linux 上则可以在 [AUR 软件包](#arch-linux-aur)发布后使用它
（在此之前，请如上所述使用 `install.sh`）。也可以按下文所述从源码构建。

<a id="verifying-a-release-by-hand"></a>
### 手动验证发布版本

每个发布版本针对每个目标（`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`）都包含
`puzuz-v<X>-<target>.tar.gz` 及其 `.sha256` 和 `.minisig` 文件；软件包 `puzuz_<X>_amd64.deb`、
`puzuz_<X>_arm64.deb`、`puzuz-<X>-1.x86_64.rpm` 和 `puzuz-<X>-1.aarch64.rpm` 也各自带有这两种文件；
另外还有 `install.sh` 及其 `install.sh.minisig`。要自己校验 tarball（或软件包）：

```sh
sha256sum -c puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
minisign -Vm puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz -p packaging/minisign.pub
```

`curl ... | sudo bash` 会直接运行下载到的内容。要先校验 `install.sh`，请将它与签名一起下载，用发布密钥
（`packaging/minisign.pub`）验证，然后运行校验过的副本：

```sh
curl -fsSLO https://github.com/kaito4681/puzuz/releases/latest/download/install.sh
curl -fsSLO https://github.com/kaito4681/puzuz/releases/latest/download/install.sh.minisig
minisign -Vm install.sh -p packaging/minisign.pub
sudo bash install.sh
```

<a id="the-release-signing-key-maintainers"></a>
### 发布签名密钥（维护者）

发布版本在 `.github/workflows/release.yml` 中使用 minisign 私钥签名，该私钥存放在仓库 secrets
`MINISIGN_SECRET_KEY` 和 `MINISIGN_PASSWORD` 中。公钥是 `packaging/minisign.pub`（它被内置到 `puzuz` 中），
以及 `scripts/install.sh` 中的 `MINISIGN_PUBKEY`。当前使用的是真正的发布密钥
（ID `21E8F80CBA3FE3CA`）。轮换密钥（密钥泄露或维护者更换）时，按下面的步骤创建新的密钥对，在同一次提交中替换
两处公钥，并更新这两个 secrets。已安装的 `puzuz` 内置的是旧公钥，无法验证用新密钥签名的发布版本，因此需要用 `install.sh` 手动重新安装：

```sh
scripts/dist/gen-signing-key.sh        # asks for a password; writes the secret key under ~
gh secret set MINISIGN_SECRET_KEY -R kaito4681/puzuz < ~/puzuz-release-key-*/minisign.key
gh secret set MINISIGN_PASSWORD -R kaito4681/puzuz
git commit packaging/minisign.pub scripts/install.sh
```

请备份私钥及其密码。启用新密钥后，需要先发布一个新版本的 `puzuz`，`puzuz update` 才会接受用新密钥签名的
发布版本。发布版本以草稿形式创建：请在 GitHub 上发布该草稿，否则 `install.sh`（它请求的是最新的已发布版本）
看不到它。

<a id="installing-the-debianubuntu-package"></a>
## 安装 Debian/Ubuntu 软件包

每个发布版本都包含面向 amd64 和 arm64 的 `.deb`（可用 `dpkg --print-architecture` 查看架构），适用于
Debian 12 以及 Ubuntu 22.04、24.04 和 26.04。从发布版本下载 `puzuz_<version>_<arch>.deb`，并用 apt
安装，apt 会同时拉取其依赖：

```sh
sudo apt install ./puzuz_0.1.0_amd64.deb
```

软件包包含：

| 路径 | |
| --- | --- |
| `/usr/bin/puzuz`, `/usr/bin/puzuz-helper` | 二进制文件 |
| `/usr/lib/systemd/system/puzuz.service`, `puzuz-helper.service` | 单元（`ExecStart=/usr/bin/...`） |
| `/usr/lib/sysusers.d/puzuz.conf`, `/usr/lib/tmpfiles.d/puzuz.conf` | `puzuz` 用户、`puzuz-admin` 组、各目录 |
| `/etc/puzuz/config.toml` | 默认配置（conffile：升级时保留你的修改） |
| `/etc/apt/sources.list.d/puzuz.sources`, `/etc/apt/keyrings/puzuz.gpg` | puzuz APT 仓库及其签名密钥（conffile，见下文） |
| `/usr/share/bash-completion`, `zsh/vendor-completions`, `fish/vendor_completions.d` | shell 补全 |
| `/usr/share/doc/puzuz` | README、本指南、架构说明、许可证 |

首次安装时，它会创建用户、组和目录，然后启用并启动 `puzuz-helper.service` 和 `puzuz.service`；
请用 `puzuz doctor` 检查结果。当 systemd 不是 PID 1 时（正在构建的容器镜像、chroot），服务只会被启用，
因此会在下次启动时运行。与 `install.sh` 不同，该软件包不会向配置中添加 `[[gpus]]` 条目；NVML
会在启动时找到 GPU（参见 [GPU 检测](#gpu-detection)）。

它依赖 systemd 240 或更高版本，并且只是**推荐**（recommends）`criu`：在大多数受支持的发行版上，
发行版自带的 CRIU 缺失或低于 4.2（参见 [CRIU](#criu)）。没有它作业也能运行，但创建检查点需要它，
`puzuz doctor` 会报告 CRIU 缺失或版本过旧。GPU 作业所需的 cuda-checkpoint 需按
[cuda-checkpoint](#cuda-checkpoint) 中所述手动安装。

如果机器上的 puzuz 是由 `puzuz service install`（或 `install.sh`）安装的，该软件包会拒绝安装：请先用
`sudo puzuz uninstall` 移除它（对于缺少该命令的旧版 puzuz，使用 `install.sh --uninstall`）。正在运行的作业、
`/etc/puzuz` 和 `/var/lib/puzuz` 都会保留。`puzuz doctor` 和 `puzuz service install-method`
会将该软件包的安装报告为发行版 `package`，而 `puzuz service install`、`install.sh` 和 `puzuz update`
会拒绝改动它。

**更新。** 软件包会注册 puzuz APT 仓库，这是位于 GitHub Release `apt` 中的一个扁平仓库
（`https://github.com/kaito4681/puzuz/releases/download/apt/`，使用 `/etc/apt/keyrings/puzuz.gpg`
中的密钥签名）。因此 `.deb` 只需手动安装一次；之后的版本会随系统的其他部分一起到来：

```sh
sudo apt update && sudo apt upgrade
```

该仓库包含 amd64 和 arm64 的稳定发布版本（不含预发布版本），每种架构保留最新的五个版本。升级会重启正在运行的
服务。与 `puzuz update` 一样，正在运行的作业会继续运行（参见[升级](#upgrading)）。你对
`/etc/puzuz/config.toml` 和 `/etc/apt/sources.list.d/puzuz.sources` 的修改会被保留；如果你没有改动
`/etc/apt/keyrings/puzuz.gpg`，它会被新版本的密钥环替换（新的签名密钥就是这样分发的）。在新发布版本上传期间，
`apt update` 可能会短暂地因哈希不匹配或文件缺失而失败；一分钟后再运行一次即可解决。

**手动添加仓库。** 除了从发布页面安装第一个 `.deb` 之外，也可以先添加仓库再从中安装。请使用以下完全一致的
文件名和内容（它们正是软件包所安装的内容，因此软件包以后不会再就它们进行询问）：

```sh
sudo curl -fsSLo /etc/apt/keyrings/puzuz.gpg \
  https://github.com/kaito4681/puzuz/releases/download/apt/puzuz.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/puzuz.sources \
  https://github.com/kaito4681/puzuz/releases/download/apt/puzuz.sources
sudo apt update && sudo apt install puzuz
```

**仓库密钥。** 软件包、仓库和 `.rpm` 文件都使用同一个 GPG 密钥签名（源码树中的
`packaging/apt/puzuz-archive-keyring.gpg`），指纹为
`E539 7032 5A39 7378 537B C776 20B6 B72F 9F55 85C2`。在信任下载的密钥之前，可以这样检查它：

```console
$ gpg --show-keys /etc/apt/keyrings/puzuz.gpg
pub   rsa4096 2026-10-13 [SC]
      E53970325A397378537BC77620B6B72F9F5585C2
uid                      puzuz APT repository <https://github.com/kaito4681/puzuz>
```

**移除。**

```sh
sudo apt remove puzuz    # stop and disable the services, remove the binaries and units
sudo apt purge puzuz     # also delete /etc/puzuz and the APT repository entry and key
```

`remove` 会保留仓库条目及其密钥（两者都是 conffile），因此 `apt update` 仍能正常工作，
`sudo apt install puzuz` 可以把 puzuz 装回来。两者都会保留 `/var/lib/puzuz`（状态、作业日志、检查点镜像）、
`puzuz` 用户和 `puzuz-admin` 组，并且从不影响正在运行的作业：它们继续在各自的 `puzuz-job-*` 单元中运行。
`purge` 会打印删除 `/var/lib/puzuz` 的方法（如果仍有作业在运行，先执行
`sudo systemctl stop 'puzuz-job-*'`，然后执行 `sudo rm -rf /var/lib/puzuz`）。

<a id="the-apt-repository-maintainers"></a>
### APT 仓库（维护者）

该仓库就是发布版本 `apt` 的资源文件集合（它是一个预发布版本，因此永远不会成为 `install.sh` 和
`puzuz update` 所跟随的“latest”发布版本）：`.deb` 文件、`Packages`、`Packages.gz`、`Release`、`InRelease`
和 `Release.gpg`、公钥 `puzuz.gpg` 以及 `puzuz.sources`。当某个发布版本**作为稳定版本发布**时
（`released` 事件：发布一个稳定版本，或将预发布版本转为稳定版本），`.github/workflows/release-apt.yml` 会通过
`scripts/dist/apt-repo.sh github <tag>` 更新它：添加该发布版本的 `.deb` 文件（先用该标签所在提交中的公钥
校验其校验和与 minisign 签名），将仓库中已有的软件包与已签名的索引核对（`InRelease` 必须能用
`packaging/apt/puzuz-archive-keyring.gpg` 验证，且每个 `.deb` 的 SHA256 必须与已签名的 `Packages` 中列出的
一致；不一致时作业失败且不做任何更改；被中断的运行留下的、索引中未列出的 `.deb` 会被丢弃），
按架构只保留最新的五个版本（按 Debian 版本顺序）并删除其余版本，
重新生成并签名索引（并从保留的最新软件包中复制出 `puzuz.gpg` 和 `puzuz.sources`），然后先上传软件包、
最后上传索引，`InRelease` 放在最末；旧的 `.deb` 文件在此之后删除。每次上传最多尝试三次；如果仍无法上传索引，
会恢复之前的索引文件，作业失败并提示重新运行。如果 `apt` 发布版本不存在，它会创建之。
发布什么由 `apt-repo.sh` 决定：草稿、预发布版本以及非 `v*` 的标签会被跳过并给出提示。索引不包含 `by-hash`
文件（它们需要子目录，而发布版本资源文件的名称不能包含 `/`），因此存在上文所述的短暂窗口。

`release.yml` 只创建草稿，因此仓库在有人于 GitHub 上发布草稿时（或某个工具用个人访问令牌发布时）才会更新。
使用工作流的 `GITHUB_TOKEN` 发布的发布版本不会触发其他工作流；这时请手动运行它，这也是重试失败上传的方法：

```sh
gh workflow run release-apt.yml -R kaito4681/puzuz -f tag=v0.1.0
```

如果想在不进行真正发布的情况下试用，请使用一个 fork：在那里用 `scripts/dist/gen-apt-key.sh` 创建密钥
（以及供 `release.yml` 使用的 minisign 密钥），提交公钥，设置 secrets，推送一个标签，发布草稿，然后针对该
fork 的 `apt` 发布版本运行 `apt update`。或者运行本地测试 `scripts/apt-repo-test.sh`
（`PUZUZ_APT_BASES="debian:12 ubuntu:22.04 ubuntu:24.04 ubuntu:26.04"`），它用
`apt-repo.sh update DIR DEB...` 和一个一次性密钥构建仓库，并在容器中通过它进行升级。

**归档密钥。** `packaging/apt/puzuz-archive-keyring.gpg` 是真正的密钥，指纹为
`E539 7032 5A39 7378 537B C776 20B6 B72F 9F55 85C2`（参见
[安装 Debian/Ubuntu 软件包](#installing-the-debianubuntu-package)中的“仓库密钥”）；其私钥和口令是仓库
secrets `APT_GPG_PRIVATE_KEY` 和 `APT_GPG_PASSPHRASE`。`release.yml` 和 `apt-repo.sh` 会拒绝用户 ID 中
写有“TEST KEY”的密钥环（由 `scripts/dist/gen-apt-key.sh --test` 生成）。创建密钥的方法如下：

```sh
scripts/dist/gen-apt-key.sh     # asks for a passphrase; writes the secret key under ~
gh secret set APT_GPG_PRIVATE_KEY -R kaito4681/puzuz < ~/puzuz-apt-key-*/apt-signing-key.asc
gh secret set APT_GPG_PASSPHRASE -R kaito4681/puzuz
# then commit packaging/apt/puzuz-archive-keyring.gpg
```

没有这两个 secrets 时，`release-apt.yml` 会跳过仓库（并给出提示）。索引只会用仓库中最新软件包所附带的密钥签名
（`apt-repo.sh` 从 `.deb` 中读取 `/etc/apt/keyrings/puzuz.gpg`），因此 secret 与已提交的公钥不匹配时，
本次运行会失败，而不会让所有人的 `apt update` 出错。

请备份私钥及其口令。每个已安装的软件包都信任该密钥，因此新密钥只能通过附带它的软件包版本，经由用旧密钥签名的
仓库来推广：

1. 创建新密钥（在一个临时检出中运行 `scripts/dist/gen-apt-key.sh`），并将其公钥追加到
   `packaging/apt/puzuz-archive-keyring.gpg`，使密钥环同时包含两个密钥
   （`cat old.gpg new.gpg >packaging/apt/puzuz-archive-keyring.gpg`）。secrets 仍保持为旧密钥，
   发布一个带有合并后密钥环的版本。
2. 等待用户升级到该版本（至少再经过一个发布周期）。
3. 将 secrets `APT_GPG_PRIVATE_KEY` 和 `APT_GPG_PASSPHRASE` 切换为新密钥，并在同一个发布版本中从密钥环里移除
   旧密钥。密钥环是 conffile：升级时 dpkg 会替换它，除非用户改动过它（此时 dpkg 会询问）。跳过了第 1 步版本的
   用户会从 `apt update` 得到 `NO_PUBKEY`，需要像首次安装那样手动安装一次下一个 `.deb`。

如果私钥泄露，请立即执行相同的步骤，并告知用户手动安装新的 `.deb`。

软件包使用 [nfpm](https://nfpm.goreleaser.com) 根据 `packaging/nfpm.yaml` 构建
（`scripts/dist/build-package.sh BIN_DIR OUT_DIR [deb|rpm]` 从构建好的二进制文件生成 `.deb` 或 `.rpm`；
`scripts/deb-test.sh` 和 `scripts/rpm-test.sh` 在容器中检查它们）。

<a id="installing-the-fedorarhelrocky-package"></a>
## 安装 Fedora/RHEL/Rocky 软件包

每个发布版本都包含面向 x86_64 和 aarch64 的 `.rpm`（可用 `uname -m` 查看架构），适用于 Fedora 以及 9
或更高版本的 RHEL 兼容发行版（RHEL、Rocky Linux、AlmaLinux；glibc 2.34 或更高版本）。用 dnf 安装，
可以从下载的文件安装，也可以直接从发布版本的 URL 安装；dnf 会拉取其依赖：

```sh
sudo dnf install ./puzuz-0.1.0-1.x86_64.rpm
# or
sudo dnf install https://github.com/kaito4681/puzuz/releases/download/v0.1.0/puzuz-0.1.0-1.x86_64.rpm
```

（像 `v0.2.0-rc.1` 这样的预发布版本对应 `puzuz-0.2.0-rc.1-1.<arch>.rpm`；在软件包内部其版本为
`0.2.0~rc.1`，排序在 `0.2.0` 之前。）

它安装的文件与 `.deb` 相同（二进制文件、`/usr/lib/systemd/system` 中的单元、sysusers.d 和 tmpfiles.d 条目、
`/etc/puzuz/config.toml`、shell 补全、文档，以及 `/usr/share/licenses/puzuz` 中的许可证），但不包含仓库配置。
配置文件是 `%config(noreplace)`：升级时保留你的修改。首次安装时，它与 `.deb` 完全一样地设置并启动服务
（当 systemd 不是 PID 1 时只启用服务），并且拒绝与由 `puzuz service install` 或 `install.sh` 进行的安装并存。
它要求 systemd 和 glibc 2.34，并且只是**推荐**（recommends）`criu`（一种弱依赖，发行版提供时 dnf 默认会安装它）：
Fedora 的 CRIU（4.2）足够新，RHEL/Rocky/Alma 9 的 3.19 则不够，因此在那些系统上创建检查点需要更新的 CRIU
（参见 [CRIU](#criu)）。没有它也可以运行作业。

**验证。** 与所有发布版本资源文件一样，`.rpm` 带有 `.sha256` 和 `.minisig` 文件（参见
[手动验证发布版本](#verifying-a-release-by-hand)）。使用 GPG 密钥构建的发布版本还会对 `.rpm` 本身签名，
并将该密钥作为 `RPM-GPG-KEY-puzuz` 附上（即 APT 仓库的密钥）：

```sh
curl -fsSLO https://github.com/kaito4681/puzuz/releases/download/v0.1.0/RPM-GPG-KEY-puzuz
gpg --show-keys RPM-GPG-KEY-puzuz                   # E53970325A397378537BC77620B6B72F9F5585C2
sudo rpm --import RPM-GPG-KEY-puzuz
rpm -K puzuz-0.1.0-1.x86_64.rpm                     # "digests signatures OK"
# or let dnf refuse an unsigned or badly signed file:
sudo dnf install --setopt=localpkg_gpgcheck=1 ./puzuz-0.1.0-1.x86_64.rpm
```

**更新。** 目前还没有 dnf 仓库（已在计划中，参见
[issue #23](https://github.com/kaito4681/puzuz/issues/23)），因此仅靠 `dnf upgrade` 看不到新版本。
请改为安装发布版本中更新的 `.rpm`；dnf 可以直接接受 URL：

```sh
sudo dnf upgrade https://github.com/kaito4681/puzuz/releases/download/v<new version>/puzuz-<new version>-1.x86_64.rpm
```

当存在更新的发布版本时，`puzuz doctor` 的“update”一行（以及 `puzuz update --check`）会打印这条命令，
其中带有适合本机架构的发布版本 URL，以及如何先校验签名。升级会重启正在运行的服务。正在运行的作业会继续运行。

**移除。**

```sh
sudo dnf remove puzuz
```

它会停止并禁用服务，移除二进制文件和单元。它会保留 `/var/lib/puzuz`（状态、作业日志、检查点镜像）、`puzuz`
用户和 `puzuz-admin` 组，并且从不影响正在运行的作业。rpm 没有 purge：修改过的 `config.toml` 会保留为
`/etc/puzuz/config.toml.rpmsave`；如果不再需要 `/etc/puzuz` 和 `/var/lib/puzuz`，请手动删除它们
（如果仍有作业在运行，先执行 `sudo systemctl stop 'puzuz-job-*'`）。

<a id="installing-with-nix"></a>
## 使用 Nix 安装

本仓库是一个 Nix flake（`flake.nix`）。它为 x86_64 和 aarch64 Linux 从源码构建 puzuz；不会从 GitHub
发布版本下载任何东西。其输出如下：

| 输出 | 内容 |
| --- | --- |
| `packages.<system>.default`（也叫 `puzuz`） | `bin/puzuz`、`bin/puzuz-helper`、bash/zsh/fish 补全、文档 |
| `packages.<system>.criu` | CRIU 4.2.1（nixpkgs 中的版本较旧），供 NixOS 模块使用 |
| `devShells.<system>.default` | 带有 Rust 工具链的开发 shell（`nix develop`） |
| `nixosModules.default` | NixOS 模块 `services.puzuz`（参见 [NixOS](#nixos)） |
| `homeManagerModules.default` | home-manager 模块 `programs.puzuz`（参见 [home-manager](#home-manager)） |
| `packages.<system>.helper-tests` | puzuz-helper 的测试可执行文件（只构建，不运行），供 `helper-tests` 检查使用 |
| `checks.<system>.*` | `home-manager`（模块求值），以及在 x86_64 上需要 KVM 的两个 NixOS VM 测试：`nixos`（该模块）和 `helper-tests`（所有 puzuz-helper 测试；由于 helper 中启动作业的测试无法在 Nix 构建沙箱内通过，软件包构建本身不运行测试） |

要跟随发布版本，请使用 `github:kaito4681/puzuz/stable`：当某个发布版本作为稳定版本发布、且是最高版本时，
`stable` 分支会移动到它的标签（绝不会移动到草稿、预发布版本或旧系列的补丁版本；见
`.github/workflows/release-nix.yml`）。
单纯的 `github:kaito4681/puzuz` 则是开发头部（`main`）。

<a id="nixos"></a>
### NixOS

flake 的 NixOS 模块（`nixosModules.default`，文件 `nix/nixos-module.nix`）会设置守护进程、root helper、
CLI、`puzuz` 系统用户、`puzuz-admin` 组、`/etc/puzuz/config.toml` 以及 `/var/lib/puzuz` 下的各目录。
不要在 NixOS 上运行 `puzuz service install` 或 `puzuz update`；两者都会拒绝执行（NixOS 通过
`/etc/os-release` 中的 `ID=nixos` 识别，参见[安装方式](#install-method)）。

将该 flake 添加到系统的 flake 中（`stable` 分支跟随最新的发布版本），并启用服务：

```nix
# flake.nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.puzuz.url = "github:kaito4681/puzuz/stable";
  inputs.puzuz.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, puzuz, ... }: {
    nixosConfigurations.gpu-server = nixpkgs.lib.nixosSystem {
      modules = [
        puzuz.nixosModules.default
        ./configuration.nix
      ];
    };
  };
}
```

```nix
# configuration.nix
{
  services.puzuz = {
    enable = true;
    admins = [ "alice" ];
    settings = {
      grace_secs = 60;
      checkpoint_size_limit_bytes = 500000000000;
    };
  };
}
```

然后执行 `sudo nixos-rebuild switch`。用 `puzuz doctor` 检查结果。

| 选项 | 默认值 | 含义 |
| --- | --- | --- |
| `services.puzuz.enable` | `false` | 安装并启动 puzuz。 |
| `services.puzuz.package` | flake 的 `puzuz` | 运行守护进程（`bin/puzuz`）和 helper（`bin/puzuz-helper`）的软件包；也会被加入 `environment.systemPackages`。 |
| `services.puzuz.admins` | `[ ]` | 加入 `puzuz-admin` 组的用户（参见[管理员](#administrators)）。 |
| `services.puzuz.settings` | `{ }` | 以属性集（attribute set）表示的 `/etc/puzuz/config.toml` 内容（各个键参见[配置](#configuration)）。 |
| `services.puzuz.criuPackage` | flake 的 `criu`（4.2.1） | 供 helper 使用的 CRIU；设置 `criu_path`。nixpkgs 自带的 `criu` 低于 4.2。 |
| `services.puzuz.cudaCheckpointPackage` | `null` | 包含 `bin/cuda-checkpoint` 的软件包；设置 `cuda_checkpoint_path`。为 `null` 时，GPU 作业无法创建检查点。 |
| `services.puzuz.nvidiaSmiPath` | `hardware.nvidia.enabled` 时为 `hardware.nvidia.package` 的 `nvidia-smi`，否则为 `null` | 设置 `nvidia_smi_path`（helper 用它查找作业的 CUDA 进程）。 |
| `services.puzuz.nvmlLibrary` | `hardware.nvidia.enabled` 时为 `hardware.nvidia.package` 的 `libnvidia-ml.so.1`，否则为 `null` | 设置 `nvml_library`，守护进程加载它来检测 GPU。 |

该模块用 `lib.mkDefault` 设置 `criu_path`、`cuda_checkpoint_path`、`nvidia_smi_path`、`nvml_library` 和
`shell`，因此 `settings` 中的值优先。NixOS 上的注意事项：

- **单元。** `puzuz.service` 和 `puzuz-helper.service` 是该模块所在 flake 修订版本中 `packaging/systemd`
  里的文件，其 `ExecStart=` 指向 store；tmpfiles.d 条目为 `packaging/tmpfiles.d/puzuz.conf`。如果你把
  `package` 设为另一个修订版本，单元仍然来自模块的修订版本；请让两者保持一致。
- **配置文件。** helper 以 root 身份运行，只读取除 root 外无人可修改的普通文件，绝不读取符号链接。
  因此该模块让 NixOS 将文件复制到 `/etc/puzuz/config.toml`（模式 0644，属于 root），而不是链接到 store。
- **`nixos-rebuild switch` 时的重启。** 修改 `settings`（或软件包）会同时重启守护进程和 helper。
  正在运行的作业不会被停止：它们运行在各自的单元中。正在重启的 helper 会先拒绝新的启动请求，并让进行中的操作
  完成，因此在检查点或恢复期间执行 switch 会等待其完成（最长为 helper 的 `TimeoutStopSec=`，即 30 分钟）。
  守护进程在此期间尝试启动或恢复的作业会被放回队列，而不是标记为失败，并在新的 helper 启动后开始运行。
- **更新与 Nix 垃圾回收。** 作业的单元运行的是启动它的那个 helper 二进制文件，其检查点镜像中也包含该二进制文件
  （恢复时需要它）。更新后，旧的 store 路径不再属于系统，因此 helper 会用一个 GC root
  `/nix/var/nix/gcroots/puzuz/job-<id>` 保持它存活，直到作业运行结束或其检查点镜像被删除为止。
  作业结束或其检查点被删除时，该 root 会被移除。随后 `nix-collect-garbage` 会释放旧版本。由早于此功能的
  helper 启动的作业或创建的检查点没有 root：旧版本被回收后再恢复它会失败，因此请在回收垃圾之前恢复或结束这类作业。
- **作业环境。** 作业以提交它的那次 `puzuz add` 的环境运行，因此其 `PATH` 是用户的 `PATH`
  （`/run/wrappers/bin`、`/run/current-system/sw/bin`、`~/.nix-profile/bin`……）。NixOS 在 `/usr/bin`
  中不放任何东西，`/bin` 中只有 `sh`；该模块设置 `shell = [ "/bin/sh" "-c" ]`，使启动作业完全不依赖 `PATH`。
- **CRIU。** 该模块通过 store 路径使用 flake 的 CRIU 4.2.1。NixOS 内核启用了 `CONFIG_CHECKPOINT_RESTORE=y`。
- **NVIDIA。** 以常规的 NixOS 方式启用 NVIDIA 驱动时（`services.xserver.videoDrivers = [ "nvidia" ]`，
  无头机器上也是如此，或者 `hardware.nvidia.datacenter.enable`），`nvidia-smi` 和 NVML 取自
  `hardware.nvidia.package`：在 NixOS 上，动态加载器不会搜索驱动所在的目录。否则请设置 `nvidiaSmiPath` 和
  `nvmlLibrary`（或 `settings.gpus`，参见 [GPU 检测](#gpu-detection)）。驱动更新后，新的 store 路径会使
  守护进程重启。
- **属于 root 的路径。** `/nix/store` 中的程序能通过 helper 和 `puzuz doctor` 的属主检查（参见
  [cuda-checkpoint](#cuda-checkpoint)）。在没有 GPU 的 VM 中，`puzuz doctor` 不会报告失败：安装方式为
  “Nix”，只有 NVIDIA 驱动和 cuda-checkpoint 的检查会给出警告（后者的提示中提到的是
  `services.puzuz.cudaCheckpointPackage`）。

<a id="cuda-checkpoint-on-nixos"></a>
#### NixOS 上的 cuda-checkpoint

cuda-checkpoint 不在 nixpkgs 中，puzuz 也不打包它（它是 NVIDIA 的二进制文件）。它的 derivation
可以像下面这样写；请选用主版本号与驱动一致的发布版本，并填入首次构建时报告的哈希：

```nix
# cuda-checkpoint.nix
{ lib, stdenv, fetchFromGitHub, autoPatchelfHook, autoAddDriverRunpath }:
stdenv.mkDerivation {
  pname = "cuda-checkpoint";
  version = "595";  # match the driver's major version
  src = fetchFromGitHub {
    owner = "NVIDIA";
    repo = "cuda-checkpoint";
    rev = "<commit>";
    hash = lib.fakeHash;
  };
  # Sets the ELF interpreter, and adds /run/opengl-driver/lib so that libcuda.so.1 is found.
  nativeBuildInputs = [ autoPatchelfHook autoAddDriverRunpath ];
  dontBuild = true;
  installPhase = ''
    install -Dm755 bin/x86_64_Linux/cuda-checkpoint $out/bin/cuda-checkpoint
  '';
  meta.license = lib.licenses.unfree;
}
```

```nix
services.puzuz.cudaCheckpointPackage = pkgs.callPackage ./cuda-checkpoint.nix { };
```

（`lib.licenses.unfree` 需要 `nixpkgs.config.allowUnfree = true` 或 `allowUnfreePredicate`。）

<a id="updating-and-removing-on-nixos"></a>
#### 在 NixOS 上更新和移除

- 更新：`nix flake update puzuz && sudo nixos-rebuild switch`。发生变化的单元会重启守护进程和 helper
  （参见上文的“`nixos-rebuild switch` 时的重启”）。正在运行的作业继续在旧的 helper 二进制文件上运行，
  更新前创建的检查点也可以恢复，即使执行了 `nix-collect-garbage` 之后也可以（参见“更新与 Nix 垃圾回收”）。
- 回滚：`sudo nixos-rebuild switch --rollback`（或启动到较旧的 generation）会恢复较旧的二进制文件，
  但不会恢复较旧的数据库：在一次迁移了数据库的更新之后，较旧的守护进程会拒绝在较新的数据库上启动。
- 移除：设置 `services.puzuz.enable = false` 并运行 `sudo nixos-rebuild switch`。单元、
  `/etc/puzuz/config.toml` 和 CLI 会被移除；`/var/lib/puzuz`（状态、日志、检查点镜像）以及 `puzuz`
  用户的文件会保留。请先终止或结束正在运行的作业：helper 会停止，但仍在运行的作业单元不会被处理。

<a id="home-manager"></a>
### home-manager

home-manager 管理的是单个用户的环境，因此它无法安装守护进程或 helper（它们需要 root 和系统的 systemd）。
其模块（`homeManagerModules.default`）在管理员已安装 puzuz 的机器上为 CLI 设置 shell 补全
（`homeManagerModules.default`，文件 `nix/home-manager-module.nix`）：

```nix
{
  imports = [ inputs.puzuz.homeManagerModules.default ];
  programs.puzuz = {
    enable = true;
    # package = null;  # the default: use the system's puzuz
  };
}
```

| 选项 | 默认值 | 含义 |
| --- | --- | --- |
| `programs.puzuz.enable` | `false` | 启用该模块。 |
| `programs.puzuz.package` | `null` | `null`：不安装任何东西，在 shell 启动时加载系统中 `puzuz`（`PATH` 中的那个）的补全：即安装在它旁边的补全文件（`<prefix>/share/...`，如 NixOS 上和各软件包中那样），没有这些文件时则使用 `puzuz completions <shell>` 的输出。若指定一个软件包（例如 `inputs.puzuz.packages.${pkgs.stdenv.hostPlatform.system}.default`），它会连同其补全文件一起安装到 profile 中。 |
| `programs.puzuz.enableBashIntegration` | `true` | 在 Bash 中加载补全（`programs.bash.enable`），配合 `package = null`。 |
| `programs.puzuz.enableZshIntegration` | `true` | Zsh 同上（`programs.zsh.enable` 配合会运行 `compinit` 的 `programs.zsh.enableCompletion`；需要 home-manager 25.05 或更高版本）。 |
| `programs.puzuz.enableFishIntegration` | `true` | Fish 同上（`programs.fish.enable`）。 |

CLI 和守护进程的版本必须相同。推荐使用 `package = null`：系统的 `puzuz` 总是与守护进程匹配。来自
home-manager（或 `nix profile`）的 `puzuz` 取自你的 home-manager flake 所锁定的修订版本，因此可能不一致；
此时命令会因版本不匹配而失败，错误消息会给出 store 路径，并且在 NixOS 上或对于 home-manager profile 会建议
使用 `programs.puzuz.package = null`。使用 `nix flake update && home-manager switch` 更新；守护进程则由管理员负责更新。

<a id="other-distributions"></a>
### 其他发行版

构建 flake，然后像使用 tarball 一样从结果安装服务：

```sh
nix build github:kaito4681/puzuz/stable
sudo ./result/bin/puzuz service install
```

`service install` 会将二进制文件复制到 `/usr/local/bin`，并且由于它们仍从 Nix store 加载库，会将该构建注册为
垃圾回收器的 root（`/nix/var/nix/gcroots/puzuz`；参见[安装](#installing)）。如果 `nix-store` 不在 root 的
`PATH` 中（sudo 的 `secure_path` 经常不包含 Nix profile），安装会给出警告，并打印让你自行注册的命令。
注册了 root 之后就可以删除 `./result`。单元运行的是 `/usr/local/bin` 中的副本，因此安装方式为
`service-install`，而不是 `nix`（后者指的是解析到 `/nix/store` 中的单元，例如 NixOS 模块的单元）。
请按 [CRIU](#criu) 中所述安装 CRIU。

要更新，请再次运行同样的两条命令（Nix 会获取当前的 `stable`；如果上次获取不到一小时，请加上 `--refresh`）；
正在运行的作业会继续运行。`sudo puzuz update` 也可以使用，但会用发布版本 tarball 中的二进制文件替换 Nix
构建的版本。使用 `sudo puzuz uninstall` 移除 puzuz（参见[卸载](#uninstalling)），它也会移除 GC root。

`nix profile install github:kaito4681/puzuz/stable` 只会为用户提供 `puzuz` 命令（带补全）（另请参见
[home-manager](#home-manager)）。helper 以 root 身份运行，无法从用户 profile 中运行，因此服务必须按上述方式安装。

<a id="arch-linux-aur"></a>
## Arch Linux (AUR)

**AUR 软件包尚未发布。** 在此之前，请在 Arch Linux 上使用 `install.sh` 安装 puzuz（它在 Arch 上可以使用：
glibc 2.34 或更高版本、systemd、x86_64 或 aarch64，并且它会校验 minisign 签名）。CRIU 4.2 或更高版本可从
官方仓库获得：

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

使用 `sudo puzuz update` 更新，使用 `sudo puzuz uninstall` 移除（参见[升级](#upgrading)和[卸载](#uninstalling)）。
本节其余部分描述的是 AUR 软件包上架 AUR 之后的情况（维护者的设置见下文）。

AUR 软件包 [`puzuz-bin`](https://aur.archlinux.org/packages/puzuz-bin)（x86_64 和 aarch64）重新打包了发布版本的
tarball。发布之后，可以用任意 AUR helper 安装它，例如 yay：

```sh
yay -S puzuz-bin
```

或者手动安装：

```sh
git clone https://aur.archlinux.org/puzuz-bin.git
cd puzuz-bin
makepkg -si
```

它会安装 `/usr/bin/puzuz` 和 `/usr/bin/puzuz-helper`、`/usr/lib/systemd/system` 中的单元、`/usr/lib` 中的
`sysusers.d` 和 `tmpfiles.d` 片段、默认的 `/etc/puzuz/config.toml`（如果你修改过，升级和移除时都会保留），
以及 bash、zsh 和 fish 的补全。pacman 的 systemd 钩子会创建 `puzuz` 用户、`puzuz-admin` 组和各目录。
按照 Arch 的惯例，服务不会自动启用或启动：

```sh
sudo systemctl enable --now puzuz-helper.service puzuz.service
puzuz doctor
```

创建检查点需要 CRIU 4.2 或更高版本，它在官方仓库中（`sudo pacman -S criu`）。GPU 作业还需要 NVIDIA 的
cuda-checkpoint（参见 [cuda-checkpoint](#cuda-checkpoint)）。两者都是该软件包的可选依赖。

- **更新：** `yay -Syu`（或用 `git pull && makepkg -si` 重新构建）。软件包不会重启服务：之后请运行
  `sudo systemctl restart puzuz-helper.service puzuz.service`，使守护进程和 `puzuz` 命令的版本再次一致。
  正在运行的作业会继续运行（参见[升级](#upgrading)）。`puzuz update` 和 `puzuz service install`
  会拒绝处理软件包安装。
- **移除：** 先执行 `sudo systemctl disable --now puzuz.service puzuz-helper.service`，然后执行
  `sudo pacman -R puzuz-bin`。仍在 `puzuz-job-*` 单元中运行的作业会继续运行，直到你停止它们。
  `/var/lib/puzuz`（状态、日志、检查点镜像）以及修改过的 `/etc/puzuz/config.toml`（保存为
  `config.toml.pacsave`）会被保留；要彻底移除，请手动删除它们。

<a id="the-aur-package-maintainers"></a>
### AUR 软件包（维护者）

PKGBUILD 位于 `packaging/aur/puzuz-bin`。当一个发布版本作为稳定版本发布时（与 APT 仓库相同的 `released`
事件；草稿和预发布版本不会触发），
`.github/workflows/release-aur.yml` 会运行 `scripts/dist/update-aur.sh <version>`，它从发布版本的 `.sha256`
文件中获取 tarball 的校验和，计算该标签源码归档的校验和（用于单元和片段），并在 Arch Linux 容器中重新生成
`.SRCINFO`。然后它将 `PKGBUILD`、`.SRCINFO` 和 `puzuz-bin.install` 推送到
`ssh://aur@aur.archlinux.org/puzuz-bin.git`。发布版本必须同时包含 x86_64 和 aarch64 的 tarball。
要在不发布新版本的情况下修改打包，请在 PKGBUILD 中递增 `pkgrel`，并手动运行工作流
（`gh workflow run release-aur.yml -f version=<X.Y.Z>`）。

一次性设置：

1. 在 <https://aur.archlinux.org> 上创建账户。
2. 创建一个专用的 SSH 密钥，并将其公钥部分添加到账户中（“My Account”，“SSH Public Key”）：

   ```sh
   ssh-keygen -t ed25519 -N '' -C puzuz-aur -f aur_puzuz
   ```

3. 将私钥部分存为仓库 secret `AUR_SSH_PRIVATE_KEY`，然后删除本地副本（或妥善保存）：

   ```sh
   gh secret set AUR_SSH_PRIVATE_KEY -R kaito4681/puzuz < aur_puzuz
   ```

第一次推送会在 AUR 上创建该软件包。没有该 secret 时，工作流只会打印一条提示。要在本地测试该软件包
（在带 systemd 的 Arch Linux 容器中测试 makepkg、pacman、服务、一个作业、升级和移除）：
`scripts/dist/aur-test.sh`。

<a id="building"></a>
## 构建

两个二进制文件都是必需的：`puzuz`（CLI 和守护进程）和 `puzuz-helper`（root helper）。单纯的
`cargo build` 只会构建 `puzuz`，因此请同时指定两个包：

```sh
cargo build --release -p puzuz -p puzuz-helper
# -> target/release/puzuz, target/release/puzuz-helper
```

如果本地没有 Rust 工具链，可以在项目的 Docker 镜像中构建（Debian bookworm 上的 Rust 1.98.1；生成的二进制文件
需要 glibc 2.34 或更高版本）：

```sh
scripts/test-in-docker.sh build --release -p puzuz -p puzuz-helper
# -> target/docker/release/puzuz, target/docker/release/puzuz-helper
```

打了标签的版本还在 GitHub 发布页面上提供面向 x86_64 和 aarch64 Linux 的预编译二进制文件（在 Debian bookworm
上原生构建，需要 glibc 2.34 或更高版本）：`puzuz-v<version>-<target>.tar.gz`（`x86_64-unknown-linux-gnu`、
`aarch64-unknown-linux-gnu`）包含 `puzuz`、`puzuz-helper` 和文档，旁边附有 `.sha256` 和 `.minisig` 文件。
`install.sh` 会安装它们（见上文）。手动安装：

```sh
sha256sum -c puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf puzuz-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo puzuz-v0.1.0-x86_64-unknown-linux-gnu/puzuz service install
```

<a id="installing"></a>
## 安装

```sh
sudo target/release/puzuz service install
```

`puzuz-helper` 必须与 `puzuz` 位于同一目录。单元从以下位置运行二进制文件：

1. 重新安装或更新时，使用当前 `service install` 安装所在的目录；
2. 否则，如果 `puzuz` 二进制文件所在目录是 `/usr/bin` 或 `/usr/local/bin`，则使用该目录
   （就地使用，不复制任何文件）；
3. 否则使用 `/usr/local/bin`，并将 `puzuz` 和 `puzuz-helper` 复制到那里（会给出提示）。这是
   管理员自行安装软件的常用位置：包管理器不会触碰它，而且它在 systemd 和所有用户的默认
   `PATH` 中。

`--bin-dir DIR` 可安装到其他位置。由于 `puzuz-helper` 以 root 身份运行，该目录及其所有父目录都
必须归 root 所有，且不能被组或其他用户写入（尚不存在的目录会以 0755 权限创建）；否则安装会拒绝，
包含 `.` 或 `..` 组成部分的路径也会被拒绝。
`--no-start` 会启用服务，但不启动或重启它们。`--force` 会覆盖并非由 `puzuz service install`
写入的单元文件、sysusers.d 和 tmpfiles.d 文件（见第 2 步）。

在 RHEL、CentOS、Rocky、Alma 和 Fedora 上，sudo 的 `secure_path` 不包含 `/usr/local/bin`，
因此 `sudo puzuz ...` 会提示 "command not found"。请运行 `sudo /usr/local/bin/puzuz ...`，或用
`sudo visudo` 将 `/usr/local/bin` 加入 `secure_path`；`puzuz doctor`（以 root 运行时）会指出这一点。
`PATH` 很精简的脚本（例如 cron 的 `/usr/bin:/bin`）也需要使用完整路径。

在 NixOS 以外用 Nix 构建时（`nix build`，然后 `sudo ./result/bin/puzuz service install`），二进制
文件会像其他构建一样被复制到 `/usr/local/bin`。它们仍从 Nix store 加载库，因此安装时还会把该
store 路径注册为垃圾回收根 `/nix/var/nix/gcroots/puzuz`（通过 `nix-store --add-root /nix/var/nix/gcroots/puzuz --realise
<store path>`）。如果找不到 `nix-store` 或它执行失败，安装会发出警告并打印该命令，之前记录的
根会被保留。是否仍需要该根，是根据已安装的二进制文件本身判断的（其动态加载器位于
`/nix/store` 中），因此用 `--bin-dir` 移动 Nix 构建的副本时它会被保留。`puzuz uninstall` 会删除
该根，安装不使用 store 的二进制文件（例如 `puzuz update`）也会删除它。

当最终的 `puzuz doctor` 报告中有 FAIL 时，`puzuz service install` 以非零状态退出。使用
`--no-start` 时，仅表示“尚未运行”的失败（两个单元、两个套接字、守护进程以及与 helper 的连接）
会被忽略。

`puzuz service install` 可以安全地重复运行（例如在升级之后）。它执行以下操作：

1. 将 `puzuz` 和 `puzuz-helper` 复制到上面选定的目录（如果那里已有内容相同、属于 root 的普通文件
   则不复制；其他情况，例如指向构建目录的符号链接，会被替换；权限 0755，原子替换）；如果它们来自 store，则注册 Nix GC 根；并写入 `/var/lib/puzuz/install-receipt`
   （root，0644）。该回执记录目录、复制的二进制文件（就地使用时为空）、版本和 GC 根，以便
   `puzuz uninstall` 准确删除这些内容。重新安装到另一个目录（使用 `--bin-dir`）时，会删除上一次
   安装复制的副本；同一目录的另一个名称（例如在 `/usr` 合并的系统上 `/bin` 之于 `/usr/bin`）视为
   同一目录，与新二进制文件是同一文件的副本永远不会被删除。如果回执存在但无法使用（不归 root 所有、可被他人写入或内容无效），安装会
   停止且不做任何更改：请修复该文件，或在检查其列出的内容后删除它。
2. 根据二进制文件内置的打包模板写入以下文件。每个文件都以一行
   "Installed by `puzuz service install`" 标记开头，并会在下次安装时被覆盖。如果其中某个文件
   已存在但没有该标记（由管理员或软件包写入），安装会在做任何更改之前停止，除非指定了
   `--force`。它创建的目录（例如 `/etc/puzuz`）无论 umask 如何都设为 0755 权限：
   - `/etc/systemd/system/puzuz-helper.service`，root helper（来自
     [packaging/systemd/puzuz-helper.service](../../packaging/systemd/puzuz-helper.service)），
   - `/etc/systemd/system/puzuz.service`，守护进程，以 `puzuz` 用户身份在严格的沙箱中运行，
     只允许访问 NVML 所需的 NVIDIA 设备（来自
     [packaging/systemd/puzuz.service](../../packaging/systemd/puzuz.service)），
   - `/etc/sysusers.d/puzuz.conf`，定义系统用户 `puzuz` 和组 `puzuz-admin`，
   - `/etc/tmpfiles.d/puzuz.conf`，定义下面列出的目录。
3. 针对该文件运行 `systemd-sysusers`，必要时运行 `groupadd --system puzuz-admin`。
4. 针对该文件运行 `systemd-tmpfiles --create`，创建：

   | 路径 | 权限、所有者 | 内容 |
   | --- | --- | --- |
   | `/var/lib/puzuz` | 0755 root | 基础目录（`data_dir`） |
   | `/var/lib/puzuz/daemon` | 0700 puzuz | `state.sqlite`，守护进程的数据库 |
   | `/var/lib/puzuz/helper` | 0711 root | helper 的作业记录和事件队列 |
   | `/var/lib/puzuz/checkpoints` | 0700 root | 检查点镜像，每个作业一个 `<id>/` |
   | `/var/lib/puzuz/logs` | 0755 root | 作业输出，`<uid>/<id>.log`（helper 以 2770 权限创建 `<uid>/`，所有者为该用户，组为 `puzuz`） |
   | `/var/lib/puzuz/lib` | 0755 root | fork guard 库（`puzuz-forkguard-<hash>.so`） |
   | `/var/lib/puzuz/criu-no-plugins` | 0755 root | 手动模式使用的空 CRIU 插件目录 |
   | `/run/puzuz` | 0755 puzuz | `puzuz.sock`（守护进程，权限 0666）和 `helper.sock`（helper，所有者 `puzuz`，权限 0600） |
   | `/run/puzuz-helper` | 0700 root | `criu restore --root` 使用的 `/` 的绑定挂载 |

5. 根据 [packaging/config.toml](../../packaging/config.toml) 创建 `/etc/puzuz/config.toml`，
   **仅当该文件尚不存在时**。如果 `nvidia-smi` 可用，则为其列出的每个 GPU 追加一个 `[[gpus]]`
   条目（作为 NVML 失败时的后备；见 [GPU 检测](#gpu-detection)）。
6. 运行 `systemctl daemon-reload`。如果单元或二进制文件有变化，则对两个服务运行
   `systemctl try-restart`。然后运行
   `systemctl enable --now puzuz-helper.service puzuz.service`，最多等待 10 秒让守护进程连接到
   helper，并打印 `puzuz doctor` 报告。

属于发行版软件包的单元文件（`/usr/lib/systemd/...`）永远不会被修改。请用 drop-in 自定义单元
（`sudo systemctl edit puzuz-helper.service`）。对 `/etc/systemd/system` 中文件的直接修改会在下次
安装时丢失。

<a id="install-method"></a>
### 安装方式

`puzuz service install` 不能与包管理器或 Nix 在同一台机器上共存：它在 `/etc/systemd/system` 中
的单元会遮蔽软件包提供的单元，而且两者都会使用 `/etc/puzuz` 和 `/var/lib/puzuz`。puzuz 通过
单元文件（分别检查 `puzuz.service` 和 `puzuz-helper.service`）区分各种安装方式：

| 安装方式 | 识别依据 |
| --- | --- |
| `puzuz service install` | 以安装标记行开头的 `/etc/systemd/system/<unit>` |
| 发行版软件包 | `dpkg-query -S`、`rpm -qf` 或 `pacman -Qo` 能给出所有者的 `/usr/lib/systemd/system/<unit>`（或 `/lib/systemd/system/...`）（两个路径都存在时都会查询，因为在 `/usr` 合并的系统上 dpkg 可能登记的是 `/lib` 路径；先询问发行版自己的包管理器）。只要有工具给出所有者，就以它为准；只有当所有已安装的工具都表示没有软件包拥有它时，该单元才视为无主。如果没有工具能回答（均未安装，或其中之一失败，例如数据库被锁定），该单元视为来源未知的软件包 |
| 无主单元 | 包管理器明确报告不属于任何软件包的此类单元（`no path found matching pattern`、`is not owned by any package`、`No package owns`）：已删除软件包的残留，或手工复制到那里的文件（`make install`） |
| Nix | 解析到 `/nix/store` 内的 `/etc/systemd/system/<unit>`（NixOS 的 `services.puzuz` 模块、system-manager），或解析到 `/nix/store` 内、或运行 `/nix/store` 中二进制文件的软件包单元 |
| 手工安装 | 没有标记的 `/etc/systemd/system/<unit>`，且没有软件包单元（其 `ExecStart=` 位于 `/nix/store` 下时也算此类） |

在软件包（或无主）单元旁边，没有标记的 `/etc/systemd/system/puzuz.service` 是管理员对它的替换
（`systemctl edit --full puzuz`），而该位置指向 `/dev/null` 的符号链接是屏蔽
（`systemctl mask puzuz`）。两者都不算第二种安装方式：puzuz 会将该软件包报告为 "overridden" 或
"masked"。没有软件包时的屏蔽会被忽略。

操作系统从 `/etc/os-release`（否则从 `/usr/lib/os-release`）的 `ID` 和 `ID_LIKE` 读取。它只决定
提示的措辞，以及 NixOS（`ID=nixos`）使用其模块；它从不决定安装方式（同一个发行版上的 puzuz
既可能来自软件包，也可能来自 `install.sh`）。不会查看 `/etc/NIXOS`。

各命令的行为（隐藏命令 `puzuz service install-method` 会打印关键字，供 `install.sh` 使用）：

| 安装方式 | `service install` | `uninstall` | `update` | `doctor` | 关键字 |
| --- | --- | --- | --- | --- | --- |
| 未安装 | 安装 | 提示未安装 | 拒绝：请使用 `install.sh` | INFO | `none` |
| 未安装，NixOS | 拒绝：请使用 `services.puzuz` 模块 | 提示未安装 | 拒绝：请使用该模块 | INFO "not installed (NixOS: use the services.puzuz module)" | `none` |
| `puzuz service install` | 重新安装 | 卸载 | 更新 | OK | `service-install` |
| 发行版软件包（dpkg、rpm、pacman 或未知） | 拒绝，即使指定 `--force` | 拒绝 | 拒绝 | OK（被屏蔽时为 INFO） | `package` |
| 无主单元 | 仅在指定 `--force` 时；建议先删除该文件 | 拒绝 | 拒绝 | WARN | `unowned` |
| Nix | 拒绝，即使指定 `--force` | 拒绝 | 拒绝 | OK | `nix` |
| 手工安装 | 仅在指定 `--force` 时 | 拒绝 | 拒绝 | INFO | `manual` |
| 混合 | 拒绝：请先卸载 `service install` 的副本 | 只删除 `service install` 写入的内容（不可用 `--purge`） | 拒绝 | FAIL | `mixed` |

“混合”是指 `service install` 与软件包或 Nix 同时存在，无论是在同一个单元上，还是分布在
`puzuz.service` 和 `puzuz-helper.service` 之间（例如守护进程单元来自 `service install`，helper
单元来自软件包）：`doctor` 失败，`service status` 将其报告为不健康，关键字为 `mixed`。
`/usr/lib` 中存在无主单元时，`service install` 仍视为 `service install`（它在 `/etc` 中的单元优先），
重新运行 `service install` 也不需要 `--force`；`doctor` 会发出警告并提示删除残留文件。当两个
单元以其他不同方式安装时（例如其中一个缺失，或来自不同包管理器的不同软件包），`doctor` 会
发出警告，`service status` 也会指出；`service install` 会安装两者，从而修复该问题。当
`/etc/systemd/system` 本身解析到 `/nix/store` 时，`service install` 也会拒绝，因为它无法写入那里。

拒绝信息会给出适合该安装方式的更新或删除 puzuz 的方法：

| 安装方式 | 更新 | 删除 |
| --- | --- | --- |
| dpkg | `sudo apt update && sudo apt upgrade` | `sudo apt remove <package>` |
| rpm | `sudo dnf upgrade` | `sudo dnf remove <package>` |
| pacman | `yay -Syu`（AUR）或 `sudo pacman -Syu` | `sudo pacman -R <package>` |
| 来源未知的软件包 | 你的包管理器（已知时根据 `ID`/`ID_LIKE` 给出名称） | 同左 |
| NixOS 上的 Nix | `nix flake update && sudo nixos-rebuild switch` | `services.puzuz.enable = false`，然后 `sudo nixos-rebuild switch` |
| 其他系统上的 Nix | 创建该单元的 Nix 配置 | 同左 |
| `puzuz service install` | `sudo puzuz update` | `sudo puzuz uninstall` |

守护进程以 `ProtectSystem=strict` 运行：它只能写入单元的
`ReadWritePaths=/var/lib/puzuz/daemon /var/lib/puzuz/logs /run/puzuz` 之下。如果在 `config.toml`
中把 `data_dir`、`state_db`、`logs_dir` 或 `api_socket` 移到别处，请在 drop-in 中添加新目录，
否则守护进程会以 "Read-only file system" 失败：

```sh
sudo systemctl edit puzuz.service
# [Service]
# ReadWritePaths=/srv/puzuz/daemon /srv/puzuz/logs
sudo systemctl restart puzuz.service
```

当配置的目录位于单元实际生效的 `ReadWritePaths=` 之外时，`puzuz doctor` 会发出警告
（`daemon write paths`）。

helper 的 `TimeoutStopSec=30min` 是系统关机时为所有作业创建检查点的初始时间预算。转储期间，
helper 会不断向 systemd 申请更多时间（`EXTEND_TIMEOUT_USEC`），但最多只到当前转储的期限
加上其回滚时间为止（见 [检查点超时](#checkpoint-timeouts)），因此卡住的转储不会无限期地阻碍
关机。作业逐个转储。对于忽略延长请求的 systemd 版本，按期限允许的速率（30 s/GiB）计算，
30 分钟大约可覆盖总计 50 GiB 的 RSS + VRAM，按实测的 NVMe 磁盘速度（约 2.5 s/GiB）则可覆盖
数百 GiB；如果你的作业更大或磁盘更慢，请在 drop-in 中调高该值。

<a id="checkpoint-timeouts"></a>
### 检查点超时

完整的检查点（对每个 CUDA 进程执行 CUDA lock + checkpoint，然后 `criu dump`）或恢复
（`criu restore`，然后 CUDA restore + unlock）必须在一个随作业大小变化的期限内完成：

```
deadline = checkpoint_deadline_base_secs + checkpoint_deadline_secs_per_gib * GiB
         = 180 s + 30 s * GiB            (defaults)
```

对于检查点，GiB 是作业的 RSS + VRAM（向上取整）；对于恢复，则是镜像大小。一个 14 GiB 的作业
得到 180 + 30 * 14 = 600 s。每个步骤也有各自的超时（lock：lock 的 `--timeout` 30 s + 15 s；
CUDA checkpoint 或 restore：300 s；unlock：60 s），`criu_dump_timeout_secs` /
`criu_restore_timeout_secs` 可以单独限制 `criu`；每个步骤取先到期的那个。

期限一过，正在运行的工具会连同其整个进程组以及它 fork 出的所有进程一起被终止。然后：

- **检查点**：作业被恢复运行——其已停止的任务收到 SIGCONT，作业 cgroup 内被冻结的 cgroup
  被解冻，已经（或可能已经）创建检查点的 CUDA 进程被 restore，每个被锁定的进程被 unlock——
  检查点以 "the checkpoint did not finish within its deadline of ... s for ... GiB of memory; the
  job was resumed" 失败。不完整的镜像会被删除。当 `criu` 被外部终止时（例如 `kill -9`、OOM
  killer），也会执行同样的回滚。每个 CUDA 进程单独回滚：期间已经退出的进程会被跳过，处于
  停止或冻结状态的进程会被保持原样（并报告），而不会阻碍其他进程。在插件模式下，CRIU 的
  CUDA 插件在 `criu dump` 内部锁定 CUDA 进程并为其创建检查点，CRIU 被终止后无法撤销，因此
  helper 会对每个进程查询 `cuda-checkpoint --get-state`，并按需 restore 和 unlock。恰好在期限
  到达的瞬间完成的 `criu dump` 视为已完成。

  注意：终止 `criu dump` 并不总能撤销。CRIU 在收集进程状态时，会在该进程内运行寄生代码
  （parasite code）；如果 CRIU 恰在此时被终止，该进程会在这段代码执行中途被释放，可能崩溃。
  因此 "the job was resumed" 表示回滚成功（作业已解冻、其 CUDA 状态已恢复），而不表示作业的
  每个进程都存活了下来；如果 `criu` 被终止，消息中会说明这一点。
- **恢复**：恢复出的进程会被终止，恢复失败；镜像会被保留。

在 NVMe 磁盘上的实测结果（RTX 5070 Ti，38 GB RAM，冷页缓存和热页缓存）：

| 步骤 | 时间 |
| --- | --- |
| CUDA lock / unlock | 0.01-0.04 s |
| CUDA checkpoint（VRAM 到主机内存） | 0.6-1.0 s/GiB（1.3-11.8 GiB VRAM；11.8 GiB：8-10 s） |
| CUDA restore | 0.5-0.9 s/GiB（11.8 GiB：6-8 s） |
| `criu dump` | 0.3-1.8 s/GiB（1-14 GiB RSS；14 GiB：18-25 s，内存压力大时更慢） |
| `criu restore` | 0.3-0.7 s/GiB（14 GiB：热缓存 8.6 s，冷缓存 9.2 s） |

一个 VRAM 11.8 GiB、RSS 1.2 GiB 的 GPU 作业（13 GiB，期限 600 s），无论手动模式还是插件模式，
创建检查点都用时 19-20 s，恢复用时 14-16 s（插件模式：`criu dump` 20-21 s，`criu restore` 16 s，
均含 CUDA 步骤）。因此作业端到端大约需要每 GiB RSS + VRAM 1.5-2.5 s；默认期限留有 10 倍以上的
余量（该作业约为 30 倍；足够应对约 35 MB/s 的磁盘）。在较慢的存储上（网络文件系统、HDD），
请调高 `checkpoint_deadline_secs_per_gib`。CUDA checkpoint 和 restore 的固定 300 s 超时，按实测
速度对每个进程最多约 30 GiB 的 VRAM 留有 10 倍余量（同样速度下，80 GB 的 GPU 也只需用掉其中
约四分之一）。

<a id="verifying"></a>
## 验证

```sh
puzuz doctor           # every check, with a `fix:` line for each problem; exits non-zero on FAIL
puzuz service status   # both units, and whether the daemon is reachable and connected to the helper
```

`puzuz doctor` 检查以下内容：配置文件及其所有权、安装方式（见 [安装方式](#install-method)）、
`criu`（4.2 或更新版本）、NVIDIA 驱动、`cuda-checkpoint`（是否支持手动模式、版本是否与驱动
匹配）、守护进程检测到的 GPU（通过 NVML 或来自 `[[gpus]]`）与 `nvidia-smi` 的对比（见
[GPU 检测](#gpu-detection)）、内核、systemd 版本、两个单元、helper 二进制文件及其上层所有目录
的所有权（取自单元实际生效的 `ExecStart=`，包括 drop-in）、守护进程的目录是否在其
`ReadWritePaths=` 之内、两个套接字、守护进程和 helper 的版本及二者的连接、检查点目录（是否
存在、是否为 tmpfs、可用空间）以及检查点总大小、你是否为管理员，以及（除非
`update_check = false`）是否有更新的发布版本。最后这项检查最多为 INFO，无法访问 GitHub 时
同样如此。它不会运行 `criu check`。请自行运行 `sudo criu check`，查看 CRIU 在此内核上支持
哪些功能。

试运行一个作业：

```sh
puzuz add -- 'for i in $(seq 1000); do echo $i; sleep 1; done'
puzuz checkpoint 0 && puzuz status
puzuz restore -i 0 && puzuz follow 0
```

<a id="configuration"></a>
## 配置

守护进程和 helper 读取 `/etc/puzuz/config.toml`。所有键都是可选的。未知的键会被忽略。文件
不存在则全部使用默认值。安装的文件（[packaging/config.toml](../../packaging/config.toml)）以注释
列出了所有键：注释行 `# key = value` 表示默认值，`# Example:` 下方缩进的行是默认未设置的键
（例如 `checkpoint_size_limit_bytes`）和 `[[gpus]]` 的示例。修改后，请重启使用该键的服务。
重启 helper 不会影响正在运行的作业：

```sh
sudo systemctl restart puzuz-helper.service puzuz.service
```

helper 以 root 身份运行，除非该文件及其所有父目录（`/etc/puzuz`、`/etc`、`/`）都归 root 所有、
不可被组或其他用户写入、且不是符号链接，否则它会**拒绝启动**。同样的规则也适用于它写入或
从中加载代码的目录：`data_dir`、`helper_state_dir`、`logs_dir`、`checkpoints_dir`、
`<data_dir>/criu-no-plugins`（传给 `criu -L`，且必须为空）、`<data_dir>/lib`（fork guard）及其
所有父目录都必须归 root 所有，且不可被组或其他用户写入（与配置文件不同，归 root 所有的符号
链接会被跟随，并检查其目标）；helper 会在其中创建任何内容之前检查它们。请保持 tmpfiles.d
创建的目录结构。守护进程把数据库放在 `daemon/` 子目录中，因为 `data_dir` 本身必须归 root 所有。

组和 GPU 槽位上限不在这里配置。它们保存在守护进程的数据库中，用 `puzuz group`、
`puzuz parallel` 和 `puzuz gpu parallel` 修改。

文件、目录和套接字的路径必须是绝对路径。程序（`criu_path`、`cuda_checkpoint_path`、
`nvidia_smi_path`、`nvml_library`、`shell` 的第一个词）要么是在 `PATH` 中查找的裸名称，要么是
绝对路径；`bin/criu` 这样的相对路径会被拒绝。helper 在启动时会在其服务的 `PATH` 中查找一次
`criu_path`、`cuda_checkpoint_path` 和 `nvidia_smi_path`，像检查绝对路径一样检查找到的文件（属于 root，
且只能经由只有 root 能修改的目录到达），此后一直使用该路径。未找到的程序在 helper 重启之前一直不可用
（其日志会说明这一点）。所有 `*_secs` 键最大为一年（31536000）。

| 键 | 默认值 | 使用者 | 含义 |
| --- | --- | --- | --- |
| `data_dir` | `"/var/lib/puzuz"` | 两者 | `daemon/state.sqlite`、`helper/`、`logs/`、`checkpoints/`、`lib/`、`criu-no-plugins/` 的基础目录 |
| `state_db` | `<data_dir>/daemon/state.sqlite` | 守护进程 | SQLite 数据库 |
| `helper_state_dir` | `<data_dir>/helper` | helper | 作业记录和事件队列（归 root 所有） |
| `logs_dir` | `<data_dir>/logs` | 两者 | 作业输出，`<uid>/<id>.log`（归 root 所有） |
| `checkpoints_dir` | `<data_dir>/checkpoints` | 两者 | 检查点镜像（归 root 所有，不能是 tmpfs） |
| `api_socket` | `"/run/puzuz/puzuz.sock"` | 守护进程 | 客户端连接的套接字（客户端一侧为 `puzuz --socket` / `PUZUZ_SOCKET`） |
| `helper_socket` | `"/run/puzuz/helper.sock"` | 两者 | helper 的套接字 |
| `daemon_user` | `"puzuz"` | helper | 唯一允许连接 helper 的用户 |
| `grace_secs` | `30` | 两者 | 停止作业时 SIGTERM 与 SIGKILL 之间的秒数 |
| `shell` | `["sh", "-c"]` | 守护进程 | 命令前缀，作业的命令行追加在其后 |
| `checkpoint_size_limit_bytes` | 未设置（无上限） | 守护进程 | 所有镜像的总大小超过该值时发出警告（镜像永远不会被自动删除） |
| `checkpoint_size_warning_interval_secs` | `3600` | 守护进程 | 超出上限期间，每位管理员收到提醒的间隔 |
| `checkpoint_tcp_close` | `true` | 两者 | 创建检查点时关闭 TCP 连接（`criu --tcp-close`） |
| `criu_path` | `"criu"` | helper | 名称（在 helper 启动时于 `PATH` 中查找）或归 root 所有的绝对路径 |
| `cuda_checkpoint_path` | `"cuda-checkpoint"` | helper | 同上 |
| `nvidia_smi_path` | `"nvidia-smi"` | helper | 同上 |
| `cuda_checkpoint_mode` | `"manual"` | helper | `"manual"`：helper 在 criu 前后调用 cuda-checkpoint。`"plugin"`：由 CRIU 的 CUDA 插件完成。 |
| `checkpoint_deadline_base_secs` | `180` | helper | 完整检查点或恢复期限的固定部分（见 [检查点超时](#checkpoint-timeouts)） |
| `checkpoint_deadline_secs_per_gib` | `30` | helper | 该期限中按每 GiB RSS + VRAM（检查点）或镜像（恢复）计算的部分 |
| `criu_dump_timeout_secs` | 未设置 | helper | 可选，单独限制 `criu dump`；未设置时只适用期限 |
| `criu_restore_timeout_secs` | 未设置 | helper | 可选，单独限制 `criu restore`；未设置时只适用期限 |
| `fork_guard` | `true` | helper | 向作业预加载 fork guard（见 README 的限制一节） |
| `gpu_device_isolation` | `true` | helper | 每个作业单元只能打开所分配 GPU 的设备节点（`DevicePolicy=closed`）；`false` 关闭此功能（见 [GPU 隔离](#gpu-isolation)） |
| `job_system_dbus` | `false` | helper | `true` 允许作业访问系统 D-Bus（systemd、polkit）；默认情况下对作业隐藏 `/run/dbus`（见 [作业能做什么、不能做什么](#what-a-job-can-and-cannot-do)） |
| `job_systemd_varlink` | `false` | helper | `true` 允许作业访问 `/run/systemd` 下的 systemd Varlink 服务；默认情况下该目录对作业隐藏，只保留 journald 的日志套接字、resolved 的名称解析套接字和 `resolv.conf` 文件以及 `userdb/`（见 [作业能做什么、不能做什么](#what-a-job-can-and-cannot-do)） |
| `job_min_uid` | `/etc/login.defs` 的 `UID_MIN`，没有则为 `1000` | helper | 拒绝 uid 小于此值的用户的作业（见 [谁可以运行作业](#who-jobs-may-run-as)） |
| `job_denied_groups` | `["docker", "lxd", "incus-admin", "libvirt", "disk", "sudo", "wheel", "admin"]` | helper | 等同 root 的组：从作业的附加组中去掉；主组为其中之一的作业会被拒绝 |
| `job_allowed_users` | `[]`（所有人） | helper | 非空时，只有列出的用户（按名称）可以运行作业 |
| `gpu_detection` | `"auto"` | 守护进程 | `"auto"`：NVML，失败时退回 `[[gpus]]`。`"static"`：只用 `[[gpus]]`。 |
| `nvml_library` | 未设置 | 守护进程 | NVML 库的路径或名称；默认先用 `libnvidia-ml.so.1`，再用 `libnvidia-ml.so` |
| `update_source` | `"github:kaito4681/puzuz"` | CLI | `puzuz update` 和 `puzuz doctor` 查找发布版本的位置：`github:<owner>/<repo>` |
| `update_check` | `true` | CLI | `puzuz doctor` 是否向 `update_source` 查询更新的发布版本 |
| `[[gpus]]` | 空 | 守护进程 | 静态 GPU 列表：`index`（必需）、`uuid`、`name`、`memory_total_bytes`（见 [GPU 检测](#gpu-detection)） |
| `[priority]` | 带 fair share 的多因子优先级 | 守护进程 | 排队作业的排序方式（见[优先级与公平共享](#priority-and-fair-share)） |

`puzuz-helper serve` 也接受命令行参数（`puzuz-helper serve --help`），它们会覆盖配置文件。请优先
使用配置文件：`puzuz doctor` 报告的是文件中的值，并会在单元的 `ExecStart=` 传递参数时发出警告。

<a id="administrators"></a>
## 管理员

管理员是 root 以及 `puzuz-admin` 的成员（主组或附加组）。他们可以操作所有用户的作业，并运行
仅限管理员的命令（`group add/remove`、`parallel`、`gpu parallel`、用 `pause/start/kill -g` 或
`-a` 暂停/恢复组、`shutdown`、对所有作业执行 `reset`，以及优先级设置 `share set`、`group priority`、`site` 和负值的
`nice`）。

```sh
sudo usermod -aG puzuz-admin alice
```

守护进程在每次请求时（通过 NSS）查询组成员关系，因此更改立即生效，无需重新登录。

<a id="who-jobs-may-run-as"></a>
## 谁可以运行作业

root helper 以所有者身份启动每个作业，因此由它决定运行谁的作业；守护进程无法改变这一点。除了从不以 root
（uid 0 或 gid 0）或所有者不属于的组运行作业之外，helper 对每次启动和每次恢复应用三项设置：

- `job_min_uid`：uid 小于此值的用户会被拒绝。未设置时为 `/etc/login.defs` 的 `UID_MIN`（大多数发行版为
  1000），文件中没有有效的 `UID_MIN` 时为 1000。这样系统账户（`www-data`、`postgres`、守护进程自己的
  `puzuz` 用户等）就不能运行作业。
- `job_denied_groups`：其成员无需密码即可以 root 身份行事的组。它们会从作业的附加组中去掉，因此属于
  `docker` 的用户的作业在没有 `docker` 的情况下运行；去掉了哪些组会记录在 helper 的日志中
  （不会改动作业的输出）。主组为其中之一的作业会被拒绝，进程带有其中之一的检查点也会被拒绝恢复（CRIU 会恢复该组）。
  每次启动和恢复时在 `/etc/group` 中查找这些名称，其中没有的名称会被忽略。gid 0 总是会被去掉。默认值及
  包含每个组的原因：

  | 组 | 为什么等同 root |
  | --- | --- |
  | `docker` | 通过 Docker 套接字可以启动绑定挂载 `/` 并以 root 运行的容器 |
  | `lxd`、`incus-admin` | 通过 LXD/Incus 可以做同样的事（特权容器、主机挂载）；不包括 Incus 受限的 `incus` 组 |
  | `libvirt` | 管理 `qemu:///system`，可以把主机的块设备和目录交给其虚拟机 |
  | `disk` | 可以直接读写块设备，也就是读写其上的所有文件 |
  | `sudo`、`wheel`、`admin` | sudo 权限：sudo 通常会询问密码，无人值守的作业无法输入，但 `NOPASSWD` 规则或缓存的凭据会让作业成为 root |

  因为不等同 root 而未包括的组：`shadow`（密码哈希，至多被离线破解）、`adm`（日志）、`kvm`，以及
  `video`/`render`（设备；GPU 作业需要后两者）。请添加在你的主机上等同 root 的组（例如 `microk8s`），或去掉
  某个组让作业重新可以使用它（例如 `job_denied_groups = ["lxd", "incus-admin", "libvirt", "disk", "sudo",
  "wheel", "admin"]` 让作业可以使用 Docker）。
- `job_allowed_users`：非空时，只有列出的用户（按名称）可以运行作业。

被拒绝的作业启动失败，原因以 `puzuz refuses to run jobs as <user>:` 开头，后面是例如 `uid 999 is below
job_min_uid 1000 (UID_MIN in /etc/login.defs)`、`primary group docker is in job_denied_groups` 或 `not in
job_allowed_users`。`puzuz status` 将其显示为 `Failed to spawn`，`puzuz log <id>` 和
`puzuz status --json` 会显示原因。被拒绝的恢复按恢复失败处理（见 [恢复失败](#restore-failures)），作业保持
已检查点状态。`puzuz doctor` 会显示生效的策略（最小 uid 及其来源、拒绝的组以及其中哪些在主机上存在、允许
的用户），helper 启动时也会将其写入日志。更改这些键后请重启 helper。

<a id="what-a-job-can-and-cannot-do"></a>
## 作业能做什么、不能做什么

作业以其所有者身份运行，可以对文件、网络和所分配的 GPU 做所有者能做的事。puzuz 保证作业无法获得更多权限，
即使所有者在 puzuz 之外可以（有 `NOPASSWD` 规则的 `sudo` 成员、`docker` 成员，或 polkit 信任的组的成员）。
从检查点恢复的作业同样如此：恢复的单元以相同方式设置，恢复的进程保持其凭据。e2e 场景 19 会在检查点和恢复
前后检查下面的每一项。

| 作业…… | 原因 | 开关 |
| --- | --- | --- |
| 以所有者的 uid 和主 gid 运行，绝不以 root 运行 | helper 拒绝 uid 0、系统账户和被拒绝的主组（[谁可以运行作业](#who-jobs-may-run-as)） | `job_min_uid`、`job_denied_groups`、`job_allowed_users` |
| 没有任何 capability（`CapInh`、`CapPrm`、`CapEff`、`CapBnd`、`CapAmb` 全为 0），且 `NoNewPrivs: 1` | shim 在运行命令前清空边界集并设置 `no_new_privs` | 无 |
| 无法通过 `sudo`、`su`、`pkexec` 或其他 setuid / 文件 capability 程序获得特权 | 在 `no_new_privs` 下内核会忽略 setuid 位和文件 capability | 无 |
| 无法访问系统 D-Bus，因此无法执行 `systemctl`、`run0`、`machinectl`、`busctl --system` 或其他对 systemd、polkit、系统服务的请求 | shim 在作业的挂载命名空间中用一个空的、属于 root 的 tmpfs 隐藏 `/run/dbus`（若 `/var/run` 不是指向 `/run` 的链接，`/var/run/dbus` 也一样），主机上还没有该目录时会先创建它，因此之后才出现的总线套接字同样无法访问；恢复时 CRIU 会保存并重建这个 tmpfs；polkit 从用户数据库而不是作业的组判断组成员关系，仅去掉组无法阻止它 | `job_system_dbus = true` |
| 无法访问 systemd 的 Varlink 服务（`io.systemd.Manager` 的 reload 和 reexecute、`io.systemd.Credentials` 对系统凭据的解密、mountfsd、nsresourced、machined、logind、networkd、sysupdate 等），这些服务不经过 D-Bus，通过 polkit 为非特权调用者授权 | shim 用一个空的、属于 root 的 tmpfs 隐藏 `/run/systemd`，只保留 journald 的日志套接字（`journal/socket`、`journal/stdout`、`journal/dev-log`）、resolved 的 `resolve/io.systemd.Resolve`、`resolve/stub-resolv.conf` 和 `resolve/resolv.conf`（逐个绑定，因此之后在那里创建的套接字仍无法访问）、`userdb/` 目录（只用于用户和组查询，见下文）以及一个空的 `system/`；恢复时 CRIU 会保存并重建这些 | `job_systemd_varlink = true` |
| 不具有 `docker`、`lxd`、`incus-admin`、`libvirt`、`disk`、`sudo`、`wheel`、`admin` 组（root 守护进程的套接字、原始块设备） | helper 将它们从附加组中去掉 | `job_denied_groups` |
| 除标准伪设备和所分配 GPU 的设备节点外，无法打开任何设备节点 | `DevicePolicy=closed` 加 `DeviceAllow=`（[GPU 隔离](#gpu-isolation)） | `gpu_device_isolation` |

每个开关都会削弱一项保证；`job_system_dbus = true` 或 `job_systemd_varlink = true` 时 `puzuz doctor` 会发出警告。puzuz 不限制的内容：所有者的
uid 本身能做的事（文件、通过会话总线的用户服务、网络访问）、作业使用的 CPU、内存或磁盘量，以及自行检查凭据的
其他服务的套接字（例如允许任何用户的守护进程套接字）。作业的挂载是私有的（不接收主机的传播），因此主机之后在
`/run/dbus`、`/run/systemd` 或其 `journal/` 和 `resolve/` 中创建的套接字，无论恢复前后都不会出现在作业中（e2e 场景 19
会检查这一点）。唯一整个绑定的目录是 `userdb/`：其中的服务只实现 `io.systemd.UserDatabase`（`GetUserRecord`、
`GetGroupRecord`、`GetMemberships`），是只读查询，回答的内容 `getent` 也能查到，因此之后加入的套接字也不会授予任何权限。
其他 root 套接字仍可访问，但不会给非特权调用者任何权限：udev 的 `/run/udev/io.systemd.Udev` 只接受 root（套接字模式为
0644）；polkit 的 `/run/polkit/agent-helper.socket` 只能完成客户端通过 D-Bus 发起的 polkit 认证（作业无法访问 D-Bus）；
系统总线不监听其他地址（只有 `<listen>unix:path=/run/dbus/system_bus_socket</listen>`）。名称解析可以正常使用，但作业看到的是
作业启动时的 `resolv.conf`（这些文件是绑定挂载的，而 resolved 在变化时会替换文件）。

<a id="priority-and-fair-share"></a>
## 优先级与公平共享

当多个排队作业等待相同的组或 GPU 槽位时，puzuz 会在能够放入的作业中启动优先级最高的那个（放不下的作业
永远不会阻塞排在它后面的较小作业）。优先级遵循 Slurm 的
[multifactor priority plugin](https://slurm.schedmd.com/priority_multifactor.html)：

```text
priority = site factor
         + priority_weight_age       * age factor
         + priority_weight_fairshare * fair-share factor
         + priority_weight_job_size  * job size factor
         + priority_weight_partition * partition factor
         + priority_weight_tres      * TRES factors
         - nice
```

每个因子都在 0 到 1 之间：

- **Age（等待时间）：** 自作业变为可运行（已排队，且所有依赖都已成功结束）以来的时间除以
  `priority_max_age_secs`，最大为 1。暂存（stashed）的作业和正在等待依赖的作业不会累计等待时间。
- **Fair share：** Slurm 的经典公式 `F = 2^(-U/S)`。`S`（归一化份额）是该用户的份额除以所有*活跃*
  用户份额之和，活跃用户指有未结束作业或有用量记录的用户（除非管理员设置了其他数值，每个用户的份额为 1）。
  `U`（有效用量）是该用户的用量除以所有用户的用量（在没有人使用过任何资源时为 0）。恰好用完自己份额的用户
  得到 0.5，用得比份额少则更高，用得多则更低；份额为 0 的用户得到 0。
- **Job size（作业大小）：** 作业的 GPU 数除以机器的 GPU 数；对于仅使用 CPU 的作业，则为一个 CPU 除以机器的
  CPU 数（puzuz 不分配 CPU，因此每个作业都按一个 CPU 计算）。当 `priority_favor_small = true` 时，改为
  `(total - requested + 1) / total`，因此最小的作业得到 1。
- **Partition：** puzuz 的组扮演 Slurm partition 的角色。管理员用 `puzuz group priority <group> <n>`
  为组设置优先级；该因子是这个优先级除以所有组中最高的优先级（所有组的优先级都为 0 时为 0）。
- **TRES：** 对于 `priority_weight_tres` 中加权的每种资源（`CPU`、`GRES/gpu`），作业的申请量除以机器的总量。

**用量。** 只有作业运行的时间会被计费：作业的计费值（来自 `tres_billing_weights`，默认为每个 CPU 1 加上
每块 GPU 10）乘以其运行的秒数，计入其所有者的用量。作业在停止运行时（被暂停、创建检查点、退出、被 kill 或
丢失时）计费，运行期间则每隔 `priority_calc_period_secs`（以及守护进程启动时）计费一次。排队、暂停、已创建
检查点或等待恢复的时间不计费，并且一次计费最多覆盖两个计算周期，因此守护进程未运行的时间不会被计费。
每次计算还会让所有用量以 `priority_decay_half_life_secs` 为半衰期衰减（0：不衰减）。调度器使用在每次计算时
以及份额变更时算出的 fair-share 因子；在此期间提交第一个作业的用户，在下一次计算之前视为没有用量。用量保存在数据库中，重启后仍然保留，并按
`priority_usage_reset_period` 的设置重置（`"now"` 在每次守护进程启动时重置；按日历的周期在本地时间 00:00
重置，按周的周期在周日重置）。低于 0.001 计费秒的用量会被丢弃。

**用户自己的顺序。** pueue 的 `-o/--priority`、`puzuz switch` 和 `puzuz top` 只对同一个用户的作业排序，
与 HTCondor 的 `condor_prio` 和 Slurm 的 `scontrol top` 一样：顺序是先排被移到最前面的作业（最近一次 `top`
的在前），然后是 `-o` 优先级较高的，然后是 id 较小的。puzuz 通过按这个顺序分配该用户等待中且可以开始
（其依赖已完成）的作业的等待时间（最长的等待时间分给第一个作业）来实现它，因此该用户的优先级之和以及其他
每个用户的位置都保持不变；等待依赖的作业的等待时间保持为 0。剩余的
平局先按该用户的顺序、再按 id 决定。**这是与 pueue 的不同之处，**在 pueue 中，`-o` 优先级较高的作业胜过所有
其他作业。`priority_type = "basic"` 可恢复 pueue 的顺序（对所有作业先按 `-o` 优先级从高到低、再按 id 从小到大；
此时 nice、site 因子、`top` 和 fair share 都被忽略）。

**命令。**

| 命令 | 谁可以使用 | Slurm |
| --- | --- | --- |
| `puzuz prio [IDS] [--json]` | 所有人 | `sprio`：按调度顺序列出每个等待中的作业及其优先级和加权后的因子（不显示命令） |
| `puzuz share [--json]` | 所有人 | `sshare`：每个用户的份额、归一化份额、原始用量和有效用量以及 fair-share 因子 |
| `puzuz add --nice N`、`puzuz nice <IDS>... <N>` | 所有者（只能调高，不能低于 0）、管理员（任意 `N`） | `--nice`、`scontrol update nice=` |
| `puzuz top <IDS>...` | 所有者（需 `enable_user_top`）、管理员 | `scontrol top` |
| `puzuz site <IDS>... <N>` | 管理员 | site 因子 |
| `puzuz share set <USER> <N>` | 管理员 | `sacctmgr modify user set fairshare=` |
| `puzuz group priority <GROUP> <N>` | 管理员 | partition 的 `PriorityJobFactor` |

与 Slurm 一样，用户只能调高 nice 值，因此无法撤销管理员设置的值。被 restart 的作业（无论是副本还是
`restart --in-place`）会保留 0 或以上的 nice 值，但不会保留负的 nice 值、site 因子或其 `top` 位置。
`puzuz prio` 按调度器的顺序列出的正是调度器所考虑的作业。`puzuz status --json` 会在作业的 `nice` 和组的 `priority` 不为 0 时显示它们。

从没有优先级功能的发布版本升级时，数据库会迁移到 schema 版本 6：已有作业保留其 `-o` 优先级，从此它只对
其所有者的作业排序，用量从零开始。旧的发布版本无法打开迁移后的数据库（`puzuz update` 会拒绝安装它们）。

**设置。** 位于 `/etc/puzuz/config.toml` 的 `[priority]` 表中（更改后请重启 `puzuz.service`）。
`[priority]` 行之后的键都属于该表，因此请把其他键放在它的上方。

| 键 | 默认值 | Slurm |
| --- | --- | --- |
| `priority_type` | `"multifactor"`（`"basic"`：pueue 的顺序） | `PriorityType` |
| `priority_weight_age` | `1000` | `PriorityWeightAge` |
| `priority_weight_fairshare` | `10000` | `PriorityWeightFairshare` |
| `priority_weight_job_size` | `0` | `PriorityWeightJobSize` |
| `priority_weight_partition` | `0` | `PriorityWeightPartition` |
| `priority_weight_tres` | `""`（例如 `"GRES/gpu=1000"`） | `PriorityWeightTRES` |
| `priority_max_age_secs` | `604800`（7 天） | `PriorityMaxAge` |
| `priority_decay_half_life_secs` | `604800`（7 天；0：不衰减） | `PriorityDecayHalfLife` |
| `priority_calc_period_secs` | `300` | `PriorityCalcPeriod` |
| `priority_usage_reset_period` | `"none"`（`"now"`、`"daily"`、`"weekly"`、`"monthly"`、`"quarterly"`、`"yearly"`） | `PriorityUsageResetPeriod` |
| `priority_favor_small` | `false` | `PriorityFavorSmall` |
| `tres_billing_weights` | `"CPU=1.0,GRES/gpu=10.0"` | `TRESBillingWeights` |
| `enable_user_top` | `true` | `SchedulerParameters=enable_user_top` |

例如，要让使用更多 GPU 的作业优先，并提升 `urgent` 组的优先级：

```toml
[priority]
priority_weight_tres = "GRES/gpu=2000"
priority_weight_partition = 5000
```

```sh
sudo puzuz group priority urgent 10
```

<a id="gpu-detection"></a>
## GPU 检测

守护进程在启动时检测一次 GPU。添加、移除或更换 GPU 后，请重启 `puzuz.service`。

- 使用 `gpu_detection = "auto"`（默认）时，它在运行时加载 NVML（`libnvidia-ml.so.1`）并忽略
  `[[gpus]]`。如果 NVML 无法加载或初始化，它会记录原因（`journalctl -u
  puzuz.service`，一条以 "GPU detection with NVML failed" 开头的警告），改用配置中的 `[[gpus]]`
  列表；如果没有该列表，则在没有 GPU 的情况下运行。
- 使用 `gpu_detection = "static"` 时，它始终使用 `[[gpus]]` 列表。

NVML 需要对 `/dev/nvidiactl` 和 `/dev/nvidia<N>` 有读写权限。这些设备节点通常为 0666 权限，因此
`puzuz` 用户不需要加入额外的组。因此打包的 `puzuz.service` 不使用 `PrivateDevices=yes`（其私有
`/dev` 中没有 NVIDIA 节点），而是使用 `DevicePolicy=closed` 并通过 `DeviceAllow=` 允许 NVIDIA
控制设备和 GPU 设备，这样守护进程只能打开这些设备和标准的伪设备。如果你自定义该单元，请保留
这些行，否则 NVML 会以 "NVIDIA driver is not loaded"（节点被隐藏）或 "internal driver error"
（节点不被允许）失败。日志消息会说明是哪一种。

`puzuz service install` 在创建配置文件时，也会根据 `nvidia-smi` 写入 `[[gpus]]` 列表作为后备。
`puzuz doctor` 会询问守护进程检测到了哪些 GPU 以及如何检测到的，并与 `nvidia-smi` 比较。在以下
情况下它会发出警告：auto 模式下 NVML 失败；`nvidia-smi` 列出了 GPU 但守护进程没有 GPU；守护
进程的 GPU（或 `[[gpus]]` 后备列表）与 `nvidia-smi` 不一致；`gpu_detection = "static"` 且
`[[gpus]]` 为空。NVML 正常工作时，`[[gpus]]` 为空也没有问题。该列表形如：

```toml
[[gpus]]
index = 0                 # PCI bus order, as nvidia-smi and CUDA_DEVICE_ORDER=PCI_BUS_ID
uuid = "GPU-..."          # nvidia-smi --query-gpu=index,uuid,name,memory.total --format=csv
name = "NVIDIA GeForce RTX 4090"
memory_total_bytes = 25757220864
```

| 字段 | 默认值 | 含义 |
| --- | --- | --- |
| `index` | 必需 | GPU 按 PCI 总线顺序的编号，与 nvidia-smi 的编号一致。必须唯一。`puzuz add --gpu` 使用它，作业在 `CUDA_VISIBLE_DEVICES` 中看到它（配合 `CUDA_DEVICE_ORDER=PCI_BUS_ID`）。 |
| `uuid` | `""` | GPU 的 UUID。如果启动时在某个编号上发现 UUID 与之前不同的 GPU，守护进程会发出警告。 |
| `name` | `""` | 型号，由 `puzuz gpu` 显示，并随每个检查点记录。恢复要求型号相同。 |
| `memory_total_bytes` | 未设置 | GPU 显存字节数，由 `puzuz gpu` 显示 |

`puzuz gpu` 显示守护进程使用的信息：使用 NVML 时，包括每个 GPU 的名称、显存和驱动版本。GPU
端到端测试会在打包的单元内检查 NVML 检测。

守护进程之前知道但现在找不到的 GPU 会继续以不可用状态列出，并保留其槽位上限。需要它的作业
保持排队，请求它的新作业会被拒绝。每个 GPU 初始上限为 1 个作业。用
`puzuz gpu parallel <N> [--gpu LIST]` 修改（`0` 表示不限）。

<a id="gpu-isolation"></a>
## GPU 隔离

`CUDA_VISIBLE_DEVICES` 只是告诉 CUDA 使用哪些 GPU，作业也可以修改它。在 `gpu_device_isolation = true`
（默认）时，helper 还会通过作业 systemd 单元的 cgroup 设备控制器，限制每个作业能打开的设备节点：

- 每个作业单元，无论是启动的（`puzuz-job-<id>.service`）还是恢复的（`puzuz-job-<id>-r<n>.service`），都设置
  `DevicePolicy=closed`。它允许标准伪设备（`/dev/null`、`/dev/zero`、`/dev/full`、`/dev/random`、
  `/dev/urandom`、`/dev/tty`、`/dev/ptmx` 以及 `/dev/pts/*` 伪终端），未列出的其他设备一律拒绝。
- 使用 GPU 的作业会获得对每块所分配 GPU 的 `/dev/nvidia<minor>`，以及 `/dev/nvidiactl`、`/dev/nvidia-uvm`、
  `/dev/nvidia-uvm-tools`、`/dev/nvidia-modeset` 和 `char-nvidia-uvm`（按主设备号指定的统一内存设备，
  以防驱动在单元启动之后才创建这些节点）的 `DeviceAllow=`（读写）。
- 仅使用 CPU 的作业完全不能打开 NVIDIA 设备。
- 恢复的作业获得其恢复所用 GPU（与检查点时编号相同）的节点。`criu restore` 以及插件模式下 CRIU 的 CUDA
  插件都在恢复单元内运行；在手动模式下，驱动会在被恢复的进程中重新打开设备节点。开启隔离时，CUDA 作业的
  检查点和恢复仍能正常工作（GPU 端到端测试会检查这一点）。

该限制适用于单元中的所有进程，包括 root。打开其他 GPU 的 `/dev/nvidia<N>` 会以 `EPERM` 失败。在作业内，
`nvidia-smi` 只显示该作业能打开的 GPU。在仅使用 CPU 的作业中，它会以 `Failed to initialize NVML: Unknown
Error` 失败，CUDA 也找不到设备。查看某个单元的设置：

```sh
systemctl show -p DevicePolicy -p DeviceAllow puzuz-job-<id>.service
```

GPU 编号遵循 PCI 总线顺序，但设备节点以驱动的次设备号命名，两者不一定相同。每次启动和恢复时，helper 从
`/proc/driver/nvidia/gpus/<PCI 地址>/information` 的 `Device Minor` 读取次设备号，并与 NVML 和 CUDA 一样，
按 PCI 总线顺序计数，跳过标记为 `GPU Excluded: Yes` 的 GPU。驱动不认识的 GPU 编号（不存在的 GPU 的
`[[gpus]]` 条目，或未加载驱动）不会获得它自己的节点，helper 会记录这一情况。

要关闭隔离，请设置 `gpu_device_isolation = false` 并重启 `puzuz-helper.service`。此后启动或恢复的作业不受
设备限制；正在运行的作业保留其单元的设置。以下情况需要关闭它：

- 使用 **MIG** 的主机：CUDA 还会打开 MIG 实例的 `/dev/nvidia-caps/nvidia-cap<N>`，而它不被允许；
- 作业需要其他设备节点，例如 `/dev/fuse`、`/dev/kvm` 或 `/dev/infiniband` 下的 RDMA 设备。

`puzuz doctor` 会显示隔离是否开启（`gpu isolation`）。

<a id="checkpoint-storage"></a>
## 检查点存储

- 镜像保存在 `checkpoints_dir`（默认 `/var/lib/puzuz/checkpoints/<job id>/`）中，只有 root 能读取。
  每个目录包含 `images/`（CRIU 镜像文件）、`dump.log`、`restore.log`、`meta.json`（GPU、驱动、
  大小、时间）以及最后写入的 `COMPLETE` 标记。没有该标记的目录是不完整的，会在 helper 启动时
  被清理，但转储已成功的目录除外（有 `DUMPED` 标记，或 CRIU 的日志表明如此）：此时作业的进程已经不在，
  镜像是它仅存的部分。helper 会在启动时或作业恢复前完成这样的目录，并且永远不会删除它。如果在转储成功后
  紧接着写入元数据失败（磁盘已满或出现故障），作业会被视为已创建检查点，helper 会记录原因，守护进程会记录
  一条警告；在元数据能够写入之前，恢复会失败，但镜像会被保留。
  helper 在系统关机时完成转储后，还会在那里写入 `shutdown-manifest.json`。
- 每个作业最多有一个镜像。恢复成功、作业被删除（`remove`、`clean`、`reset`）或转储失败时，
  镜像会被删除。恢复失败时镜像会被保留。
- 转储失败后，helper 会把其 `dump.log` 移到同一目录下的 `failed-logs/<job id>-<unix ms>.log`
  （仅限 root），并保留最新的 20 个。作业的最后错误会给出该文件名。
- 空间：每次转储前，helper 要求可用空间至少为（作业的 RSS + VRAM + 其私有 `/tmp` 和 `/dev/shm` 中
  存储的内容）× 1.2。否则检查点会被拒绝，
  作业继续运行。镜像不压缩。测试中，一个 VRAM 为 7 GB 的作业生成了 8.1 GB 的镜像。不要使用
  tmpfs（`puzuz doctor` 会对其报告失败）。当可用空间低于 20 GiB，或低于
  `checkpoint_size_limit_bytes` 下的剩余空间时，它也会发出警告。
- 上限：设置 `checkpoint_size_limit_bytes` 后，使总量超出上限的那个检查点的所有者会收到一次
  警告。管理员会在执行命令时收到警告，间隔至少为 `checkpoint_size_warning_interval_secs`，
  `puzuz doctor` 也会报告。转储不会被拒绝，也不会自动删除任何内容。管理员可在 `puzuz status`
  中看到所有作业的镜像大小。
- 要使用另一块磁盘，请创建一个归 root 所有的目录（权限 0700，所有父目录归 root 所有且不可被
  组或其他用户写入），设置 `checkpoints_dir`，并确保关机时 helper 在该文件系统卸载之前停止：

  ```sh
  sudo install -d -o root -g root -m 0700 /data/puzuz-checkpoints
  sudo systemctl edit puzuz-helper.service
  #   [Unit]
  #   RequiresMountsFor=/data/puzuz-checkpoints
  sudo systemctl restart puzuz-helper.service puzuz.service
  ```

  更改路径之前，请先移动或恢复现有的检查点。

<a id="cuda-plugin-mode"></a>
## CUDA 插件模式

默认情况下（`cuda_checkpoint_mode = "manual"`），helper 自行运行 cuda-checkpoint（`lock`、
`checkpoint`，然后 `criu dump`；`criu restore`，然后 `restore` 和 `unlock`），并让 CRIU 使用一个空
的插件目录。在插件模式下，它只运行 `criu`，由 CRIU 的 CUDA 插件在转储和恢复的适当时机调用
cuda-checkpoint。启用方法：

1. 构建带插件的 CRIU 4.0 或更新版本（puzuz 使用 4.2.1 测试），并把插件安装到 CRIU 内置的插件
   目录 `/usr/lib/criu`。在插件模式下 helper 不传递 `-L`，因此 CRIU 会在那里查找。

   ```sh
   git clone --branch v4.2.1 https://github.com/checkpoint-restore/criu && cd criu
   make -j criu cuda_plugin
   sudo install -m 0755 criu/criu /usr/local/sbin/criu
   sudo install -D -m 0755 plugins/cuda/cuda_plugin.so /usr/lib/criu/cuda_plugin.so
   ```

2. 把 cuda-checkpoint 放到 `/usr/local/bin`（或 systemd 服务默认 `PATH` 中的其他位置）。插件按
   名称运行 `cuda-checkpoint`，会忽略 `cuda_checkpoint_path`。
3. 在 `/etc/puzuz/config.toml` 中设置 `cuda_checkpoint_mode = "plugin"`，运行
   `sudo systemctl restart puzuz-helper.service`，并用 `puzuz doctor` 检查（`cuda plugin` 一行）。

每个镜像在 `meta.json` 中记录其模式，并以该模式恢复，因此更改 `cuda_checkpoint_mode` 后现有的
检查点仍然可用。恢复插件模式的镜像需要该插件，因此在这些镜像恢复之前请保留插件。如果插件
缺失，helper 会在插件模式下拒绝转储 CUDA 作业（作业继续运行），而不是让转储失败。仅使用 CPU
的作业不需要插件。

权衡：

| | manual（默认） | plugin |
| --- | --- | --- |
| CRIU | 任何 CRIU 4.2 或更新版本，包括不带插件的发行版软件包 | 带 `cuda_plugin.so` 构建的 CRIU |
| 恢复时的 GPU | 型号和驱动相同；同型号的另一张卡会被重新映射（`--device-map`） | 型号和驱动相同，且 GPU UUID 相同（CRIU 4.2.1 的插件无法重新映射） |
| 超时 | 整个操作的期限，加上每个 cuda-checkpoint 步骤（lock、checkpoint、restore、unlock）的超时 | 仅整个操作的期限（它涵盖插件在 `criu` 内部对 cuda-checkpoint 的调用） |
| 失败处理 | helper 回滚它已执行的 CUDA 步骤（restore 并 unlock） | 转储失败时由 CRIU 的插件恢复进程运行 |

对用户而言两种模式行为相同。在两种模式下，都是 fork guard 使从 CUDA 进程 fork 出的 PyTorch
DataLoader worker 能够创建检查点：没有它，插件模式同样会在这些进程上失败
（`handle_device_vma plugin failed`、`Can't handle non-regular mapping`）。无论哪种模式，fork guard
都会预加载到每个作业中。两种模式下，暂停的作业都会为转储而恢复运行，因为 cuda-checkpoint 在
已停止的进程上会挂起。

<a id="filesystems-and-mounts"></a>
## 文件系统和挂载

每个作业都在自己的挂载命名空间中运行。作业启动时，shim 把主机的挂载复制到其中并加以修正，
使作业看到的内容就是 CRIU 转储并在恢复时重建的内容：

- 自动挂载点（`x-systemd.automount`、autofs 直接映射）会在作业运行前触发一次，作业的命名空间
  得到的是已挂载的文件系统本身，而不是自动挂载点。如果自动挂载的文件系统在 20 s 内没有出现
  （NFS 服务器无响应），则保持原样，作业无法使用该路径。
- 从此以后该命名空间是私有的：管理员之后挂载或卸载的文件系统不会出现在正在运行的作业中。
  要使用它们，请启动新作业。
- `/run` 和 `/media` 下的挂载，以及被其他挂载遮盖的挂载，会被移除。每个作业在 `/tmp` 和
  `/dev/shm` 上有自己的 tmpfs，并会保存在检查点中。

CRIU 在恢复时通过绑定挂载相同的主机路径来重建作业命名空间中的每个挂载，因此**作业启动时已
挂载的每个文件系统，都必须在相同路径以相同类型重新挂载，作业才能恢复**，即使作业从未使用
过它。该文件系统可以作为新的挂载回来（不同的设备号、新的 loop 设备、新的 NFS 挂载）。转储时，
helper 会记录这些路径各自的文件系统类型和挂载点，以及作业打开的文件和工作目录。恢复前，它会
检查这些是否仍然存在（途中触发自动挂载点），否则以明确的错误拒绝（见
[恢复失败](#restore-failures)）。镜像会被保留，因此你可以挂载该文件系统后再次运行
`puzuz restore`。

在恢复出的作业运行之前，helper 会检查其所有者能否以作业打开每个文件的同样方式、通过同样的
路径且不经过符号链接打开这些文件（见 [恢复与路径](architecture.md#restores-and-paths)）。对于
作业在转储时已经打开的文件，即使其所有者现在无法以那种方式打开它（作业以只读方式创建的文件，
或所有文件都显示为 root 所有的 sshfs 挂载；也包括它在转储前一刻才打开的文件），只要它确实是同一个文件，
检查也会通过：

- 在同一个已挂载的文件系统上（期间没有重启或重新挂载）：设备和 inode 相同；或
- 文件系统重新挂载之后：所有者、组、权限、大小、修改时间和变更时间（ctime）与转储刚结束时
  相同。设备号会随新挂载而改变，而 inode 号并非在所有文件系统上都稳定：sshfs 等 FUSE 文件系统
  按查找顺序为 inode 编号，因此重启后作业的文件通常会有其他编号（另一个文件可能拿到旧编号）。
  变更时间由内核（在 sshfs 上由服务器）设置，作业的所有者无法在不属于自己的文件上设置它。

因此，在检查点作业恢复之前不要动它的文件：时间发生变化的文件会像其他文件一样按其所有者的
权限检查，而对于将文件显示为 root 所有的 sshfs 挂载（`root@server:`、
`allow_other,default_permissions`），恢复随后就会被拒绝。在所有者可以像作业那样打开文件的
文件系统上（通常情况），文件的变化无关紧要。

已在模拟重启中测试（`scripts/e2e-test.sh`，场景 15 和 16），作业的工作目录和打开的文件分别位于
以下各种挂载上：

| 挂载 | 重启后恢复 |
|---|---|
| 挂载在固定路径的独立本地文件系统（loop 设备上的 ext4），重启后位于另一设备上 | 可行 |
| 绑定挂载（来自另一文件系统的 `/home/<user>`）、只读绑定挂载、private 传播的绑定挂载；shared 传播的 `/`（systemd 默认） | 可行 |
| overlayfs | 可行 |
| `x-systemd.automount`（作业在触发前和触发后启动；恢复时尚未触发） | 可行：恢复的预检查会触发它 |
| FUSE 网络文件系统（sshfs，由 root 以 `allow_other,default_permissions` 挂载，因此文件显示为 root 所有），作业在其中写入和重命名自己的文件，且重启后 inode 号不同 | 只要 FUSE 守护进程在恢复前重新运行即可 |
| 重启后未挂载的文件系统（缺少 fstab 条目、NFS 服务器宕机） | 拒绝：`<path> needs <type> on <mount point>` |
| 管理员挂载的 tmpfs（例如 `/scratch`），作业的文件或工作目录位于其上 | 拒绝：重启后其内容已丢失。只把 tmpfs 用于不需要跨重启保留的作业。同一次启动内的恢复可行。 |

未测试或不支持：

- **内核 NFS**（NFSv3/v4）未测试：Docker 测试环境无法加载主机的 `nfs` 模块。在上述检查中，NFS
  与其他文件系统表现相同。请从 `/etc/fstab` 挂载它（`_netdev` 或 `x-systemd.automount`），使其
  在 helper 启动前挂载，并在关机时 helper 转储完作业之后才卸载（`puzuz-helper.service` 排在
  `remote-fs.target` 之后）。作业打开的文件会按路径重新打开，因此它们必须仍存在于服务器上。
- **autofs 间接映射**（autofs 守护进程按需挂载 `/net/<host>` 或 `/home/<user>`）：作业启动时已
  挂载的键对其可见；之后才首次访问的键不可见（命名空间是私有的）。恢复这样的命名空间尚未
  测试。
- **可移动介质和 `/run`**：对作业完全不可见。
- 重启后无法在相同路径重新挂载的文件系统（路径中含有 boot ID 或随机名称），会使在其挂载期间
  启动的所有作业都无法恢复。

<a id="upgrading"></a>
## 升级

如何更新取决于 puzuz 的安装方式（`puzuz doctor` 将其显示为 "install method"，其 "update" 行会在
有更新的发布版本时提示）：

| 安装方式 | 更新方式 |
| --- | --- |
| `install.sh`（或发布版本的 `puzuz service install`） | `sudo puzuz update` |
| `.deb` | `sudo apt update && sudo apt upgrade`（或用 `sudo apt install ./puzuz_<new version>_<arch>.deb` 安装发布版本中更新的 `.deb`） |
| `.rpm` | `sudo dnf upgrade <URL of puzuz-<new version>-1.<arch>.rpm>`（openSUSE 上为 `sudo zypper install <URL>`；尚无 dnf 仓库） |
| 从源码构建 | `cargo build --release -p puzuz -p puzuz-helper`，然后 `sudo target/release/puzuz service install` |
| 来自 Nix 构建的 `puzuz service install`（非 NixOS） | 再次执行相同的 `nix build` 和 `service install`（见 [使用 Nix 安装](#installing-with-nix)） |
| AUR 软件包 `puzuz-bin`（发布之后；在此之前 Arch 使用 `install.sh` 和 `sudo puzuz update`） | `yay -Syu`（任意 AUR helper），然后 `sudo systemctl restart puzuz-helper.service puzuz.service`；`puzuz update` 会拒绝 |
| NixOS（`services.puzuz`） | `nix flake update puzuz && sudo nixos-rebuild switch`（见 [NixOS](#nixos)）；`puzuz update` 会拒绝 |
| 其他发行版软件包 | 包管理器（见 [安装方式](#install-method)）；`puzuz update` 会拒绝并给出相应命令 |

<a id="puzuz-update"></a>
### puzuz update

```sh
puzuz update --check          # is there a newer release? (any user; without root, update only checks)
sudo puzuz update             # install the latest release
sudo puzuz update --version 0.2.0 --yes
```

`puzuz update` 不需要 GitHub token。与 `install.sh` 一样，如果提供了 token 它就会使用（可提高 API
速率限制）：`GH_TOKEN` 或 `GITHUB_TOKEN`（`sudo GH_TOKEN=... puzuz update`），或者由
`install.sh --save-token` 写入的 `/etc/puzuz/github-token`（root，0600）。然后它会：

1. 检查 puzuz 是否由 `puzuz service install` 安装（而不是由软件包、Nix 或手工安装，且两个单元都
   属于它：以其他方式安装的 helper 单元会在下载任何内容之前被拒绝，缺失的 helper 单元由安装写入），
   以及它是否作为已安装的二进制文件运行（`puzuz.service` 的 `ExecStart=` 中的目录），
2. 查找最新的发布版本（或 `--version` 指定的版本）并与自身版本比较。已是最新时什么也不做。
   只有通过 `--version` 明确指定时才会安装较旧的版本，
3. 请求确认（`-y/--yes` 跳过确认），
4. 将 tarball、`.sha256` 和 `.minisig` 下载到 `/var/lib/puzuz/update`（root，0700），用 `puzuz`
   内置的发布密钥验证 minisign 签名，并校验校验和，
5. 解压 tarball（仅限普通文件和目录），并询问新的 `puzuz` 支持哪个数据库模式（schema）。无法
   打开守护进程数据库的发布版本（模式变更之前的旧版本）会在任何更改之前被拒绝，
6. 针对已安装的目录运行新的 `puzuz service install`，删除 Nix 构建安装的 Nix GC 根（发布版本的
   二进制文件不需要 store），等待守护进程运行新版本并连接到 helper，然后删除下载的文件。如果
   `service install` 失败，`puzuz update` 会检查已安装的二进制文件：若它们不是该发布版本的文件，
   就以该错误停止；若是（只有最后的检查失败），就发出警告并继续。

`install.sh`（再次运行，可选 `--version`）同样可用，效果相同；它用 `PATH` 中的 `minisign` 验证签名，
或者下载一个固定版本的 minisign（参见[运行安装程序](#running-the-installer)）。

`service install`（因此上述两种方式都是）会替换二进制文件和单元，并在有任何变化时重启两个
服务。配置文件会被保留。

- **正在运行的作业不受影响。** 它们运行在各自的 systemd 单元中（`puzuz-job-<id>.service`，恢复
  后为 `puzuz-job-<id>-r<n>.service`）。新的守护进程从数据库中读取状态，新的 helper 会重新找到
  正在运行的作业。
- **helper 重启：** 单纯的 `systemctl stop/restart puzuz-helper`（不是系统关机）永远不会创建
  检查点。正在停止的 helper 会拒绝新的启动和恢复，并先让进行中的操作完成。进行中的转储可能
  需要一段时间，第二次 SIGTERM 会让它立即退出。它拒绝或放弃的启动会被守护进程重新排队，并在
  helper 恢复后重试。helper 停止期间，`pause`、`kill`、`checkpoint` 以及恢复暂停的作业会以
  "helper unavailable" 失败。`add -i`、`start`、`restore -i` 和 `restart -i` 会被接受，作业会在
  helper 重新连接时启动。
- **版本：** CLI 和守护进程必须是相同版本。版本不匹配的客户端会收到错误，指出应更新哪一方，
  `puzuz doctor` 也会就此发出警告。请同时更新用户运行的 `puzuz` 二进制文件。
- `/var/lib/puzuz/lib` 中旧的 fork guard 库会被有意保留：升级前创建检查点的作业需要完全相同的
  文件才能恢复。
- 升级 NVIDIA 驱动之前，请恢复或删除已创建检查点的作业。驱动版本不同的恢复会被拒绝。

<a id="uninstalling"></a>
## 卸载

在 NixOS 上，请改为设置 `services.puzuz.enable = false` 并运行 `sudo nixos-rebuild switch`（见
[在 NixOS 上更新和删除](#updating-and-removing-on-nixos)）。

对于 `.deb`，使用 `sudo apt remove puzuz` 或 `sudo apt purge puzuz`（见
[安装 Debian/Ubuntu 软件包](#installing-the-debianubuntu-package)）；对于 `.rpm`，使用
`sudo dnf remove puzuz`（见
[安装 Fedora/RHEL/Rocky 软件包](#installing-the-fedorarhelrocky-package)）；对于 AUR 软件包（发布
之后），使用 `sudo pacman -R puzuz-bin`（见 [Arch Linux（AUR）](#arch-linux-aur)）。目前在 Arch Linux
上，以及对于由 `install.sh` 或 `puzuz service install` 完成的安装：

```sh
sudo puzuz uninstall            # stop, disable and remove the units, snippets and copied binaries
sudo puzuz uninstall --purge    # also delete /etc/puzuz, /var/lib/puzuz, /run/puzuz, /run/puzuz-helper
```

`puzuz service uninstall` 是同一个命令，`install.sh --uninstall [--purge] [--yes]` 会运行它。
`--purge` 会在终端上请求确认。没有终端时，请传递 `--yes`。

- 只删除以安装标记开头的文件。其他文件会被报告并保留。
- 只有当 `/var/lib/puzuz/install-receipt` 将二进制文件列为由 `puzuz service install` 复制时（仅限
  记录目录中的 `puzuz` 和 `puzuz-helper`），才会删除它们。就地使用的二进制文件（例如在
  `/usr/bin` 中）会被保留。在回执机制出现之前完成的安装会保留其二进制文件，`uninstall` 会打印
  需要手工删除哪些文件；回执无法使用时也是如此。列出的二进制文件如果现在已归某个软件包所有
  （向 dpkg、rpm 和 pacman 查询），会被保留；包管理器无法判断其所有者的文件也会被保留。Nix
  GC 根和回执也会被删除；如果某个二进制文件无法删除，回执会保留并继续列出它（稍后重新运行
  `sudo puzuz uninstall`）。
- 如果已安装的 `puzuz` 版本早于 `puzuz uninstall` 出现的版本，`install.sh --uninstall` 会运行其
  `service uninstall`，然后从单元所在的目录中删除 `puzuz` 和 `puzuz-helper`。
- 正在运行的作业会在各自的单元中继续运行。`uninstall` 会就此发出警告，而只要有任何
  `puzuz-job-*` 单元处于活动状态，或 systemd 无法列出它们，`--purge` 就会拒绝运行。请先用
  `sudo systemctl stop 'puzuz-job-*'` 停止它们，或在卸载前为它们创建检查点或终止它们。
- `--purge` 会请求确认（`-y/--yes` 跳过确认）。它不会进入挂载点（例如 `/run/puzuz-helper` 下
  `/` 的绑定挂载）。这些挂载点会被报告，由你手工卸载并删除。本身就是挂载点的待删除目录（例如位于
  独立文件系统上的 `/var/lib/puzuz`）会被清空并保留。删除过程从不跟随符号链接（包括用户放在自己
  日志目录中的符号链接）。
- 系统用户 `puzuz` 和组 `puzuz-admin` 始终会被保留。

<a id="troubleshooting"></a>
## 故障排查

<a id="where-to-look"></a>
### 查看哪里

```sh
journalctl -u puzuz -u puzuz-helper          # daemon and helper
journalctl -u 'puzuz-job-*'                  # job units (start/stop, OOM kills)
puzuz status; puzuz log <id>                 # job state, last error, output
sudo ls /var/lib/puzuz/checkpoints/<id>/     # dump.log, restore.log (CRIU logs, root only)
sudo ls /var/lib/puzuz/checkpoints/failed-logs/   # dump.log of failed dumps (newest 20)
```

作业输出写入 `/var/lib/puzuz/logs/<uid>/<id>.log`。检查点或恢复失败时，作业的最后错误（由
`puzuz status` 显示，并出现在 `puzuz log` 的头部，仅对所有者和管理员可见）包含 CRIU 日志的简短
摘要。转储失败后，它还会给出完整日志的路径，例如
`(CRIU log: /var/lib/puzuz/checkpoints/failed-logs/12-1760000000000.log)`。守护进程以 `info` 级别
记录日志。如需更多细节，请在 `puzuz.service` 的 drop-in 中设置 `Environment=RUST_LOG=debug`。

如果守护进程没有运行，所有命令都会以 "cannot connect to the daemon socket" 失败。请检查
`puzuz service status` 和 `systemctl status puzuz`。执行 `puzuz shutdown` 后，守护进程会保持停止，
直到 `sudo systemctl start puzuz` 或下次启动。

<a id="common-puzuz-doctor-failures"></a>
### 常见的 `puzuz doctor` 失败

| 检查项 | 消息 | 解决方法 |
| --- | --- | --- |
| `config` | not owned by root or group/world-writable | `sudo chown root:root /etc/puzuz/config.toml /etc/puzuz; sudo chmod go-w /etc/puzuz/config.toml /etc/puzuz` |
| `config` | invalid configuration | 修正 TOML。配置无效时守护进程不会启动。 |
| `config` | not readable by others / not searchable by others (WARN) | `sudo chmod 755 /etc/puzuz; sudo chmod 644 /etc/puzuz/config.toml`：守护进程以 `puzuz` 身份运行 |
| `criu` | not found, or too old | 安装 CRIU 4.2 或更新版本，或设置 `criu_path` |
| `criu`、`cuda-checkpoint`、`gpus` | was not run as root（root 以外的用户可以替换它，或配置文件未通过所有者检查） | 让程序及其目录归 root 所有且不可被组或其他用户写入，或先修复 `config` 检查的问题 |
| `cuda-checkpoint` | not found, or lacks `--action lock/checkpoint/restore/unlock` | 将适用于 r570 或更新驱动的二进制文件安装到 `/usr/local/bin` |
| `cuda-checkpoint` | version X but the driver is Y | 使用与驱动主版本匹配的 cuda-checkpoint |
| `cuda plugin` | `cuda_checkpoint_mode` is `"plugin"` but `/usr/lib/criu/cuda_plugin.so` is missing | 安装插件（见 [CUDA 插件模式](#cuda-plugin-mode)）或使用手动模式 |
| `gpus` | nvidia-smi lists N GPU(s), the configuration M | 更新 `[[gpus]]`（见 [GPU 检测](#gpu-detection)） |
| `kernel` | lacks `/proc/sys/kernel/ns_last_pid` | 使用启用了 `CONFIG_CHECKPOINT_RESTORE=y` 的内核 |
| `update` | puzuz X is available (INFO) | `sudo puzuz update`；`update_check = false` 可关闭该检查 |
| `install method` | mixed: two install methods at once | 按 `fix:` 行的提示，只保留一种（例如用 `sudo puzuz uninstall` 保留软件包） |
| `install method` | WARN: a unit no package owns, a leftover unit, or units installed in different ways | 按 `fix:` 行的提示，删除无主的 `/usr/lib` 单元（`sudo rm ...`），或重新运行应当拥有两个单元的那种安装 |
| `puzuz.service`、`puzuz-helper.service` | not installed / not active | `sudo puzuz service install`，或 `sudo systemctl start <unit>` 并查看 `journalctl -u <unit>` |
| `helper binary` | the binary or a directory above it is not owned by root or is writable by others | `sudo chown root:root <path>; sudo chmod 755 <path>`（及其所在目录），或重新安装到另一个目录 |
| `sudo PATH` | sudo's secure_path does not include /usr/local/bin (INFO, RHEL family) | `sudo /usr/local/bin/puzuz ...`，或用 `sudo visudo` 将 `/usr/local/bin` 加入 `secure_path` |
| `daemon write paths` | cannot write to a configured directory: not below its ReadWritePaths= | 为 `puzuz.service` 添加 `ReadWritePaths=` drop-in（见 [安装](#installing)） |
| `helper` | the daemon is not connected to the helper | `journalctl -u puzuz-helper`。通常是配置文件或数据目录的所有权问题。 |
| `daemon` | runs version X while this puzuz is Y | 升级后运行 `sudo systemctl restart puzuz-helper.service puzuz.service`，或更新 CLI |
| `checkpoint dir` | on tmpfs, or little free space | 将 `checkpoints_dir` 移到有足够空间容纳作业 RAM + VRAM 的磁盘上 |
| `helper settings` | ExecStart= passes flags | 将设置移入 `config.toml` 并去掉这些参数 |

在 NixOS 上（`/etc/os-release` 中为 `ID=nixos`），`fix:` 行给出的是[模块选项](#nixos)，而不是在 NixOS
上不适用的步骤，并且之后都要运行 `sudo nixos-rebuild switch`：用 `services.puzuz.settings` 代替编辑
`/etc/puzuz/config.toml`（该文件由模块重写），用 `services.puzuz.criuPackage` 和
`services.puzuz.cudaCheckpointPackage` 代替把 CRIU 或 cuda-checkpoint 安装到 `/usr/local/bin`，
nvidia-smi 和 NVML 用 `services.puzuz.nvidiaSmiPath` 和 `services.puzuz.nvmlLibrary`，用
`services.puzuz.enable = true` 代替 `sudo puzuz service install` 或 `systemctl enable`，用
`services.puzuz.admins` 代替 `usermod`，用 `systemd.services.puzuz.serviceConfig.ReadWritePaths`
代替 `systemctl edit` drop-in。

<a id="checkpoint-failures"></a>
### 检查点失败

helper 在转储作业之前会先检查它。如果检查失败，转储会被**拒绝**：作业不受影响并继续运行，
`puzuz checkpoint` 以 `checkpoint refused: <reason>; the job
keeps running` 失败，原因成为作业的最后错误。如果拒绝在转储到期时才发生（例如在
`puzuz shutdown` 期间，此时作业会继续运行），所有者会在下次执行命令时收到警告。拒绝的原因：

- `not enough free space for the checkpoint: need N bytes, M available`：释放空间或移动
  `checkpoints_dir`，然后再次创建检查点。所需空间包括作业存储在其自己的 `/tmp` 和 `/dev/shm` 中的内容
  （此时消息会给出具体大小）。
- `the job has Unix sockets bound to [...] on the host's filesystems (or to relative names ...)`：
  恢复会让 CRIU 以 root 身份创建、unlink 和 chown 这些路径。请把套接字绑定到 `/tmp` 下的绝对路径或
  抽象名称。相对名称会被视为主机路径，因为无法得知它是在哪个目录中绑定的。
- `the job has internet sockets with SO_REUSEPORT (...)`：CRIU 在把凭据交还给作业之前以 root 身份重建
  套接字，因此恢复后的 `SO_REUSEPORT` 套接字将属于 root，并可能加入同一端口上某个 root 服务的组（并接收其
  部分连接）。请在程序中关闭 `SO_REUSEPORT`，或用 `--no-checkpoint` 运行该作业。
- `the job's mounts [...] are only reachable through mounts hidden under other mounts`：作业的
  挂载命名空间依赖一个被另一个挂载遮盖的挂载，因此永远无法恢复。请重新运行该作业
  （`puzuz restart`）。新作业会得到清理过的命名空间。
- 覆盖 `/` 本身的挂载无法处理。puzuz 会拒绝此类转储。
- `<dir> is on tmpfs`：将 `checkpoints_dir` 移到磁盘上。
- `cuda_checkpoint_mode is "plugin" but CRIU's CUDA plugin ... is missing`：安装插件（见
  [CUDA 插件模式](#cuda-plugin-mode)）或切换到手动模式。

为暂停的作业创建检查点时，它会为转储而恢复运行。如果转储被拒绝，它会再次被暂停。

已尝试但**失败**的转储会停止作业（SIGTERM，`grace_secs` 后 SIGKILL），因为 CRIU 和
cuda-checkpoint 已经冻结了它。作业被标记为 `Checkpoint failed`，并附带 CRIU 日志摘要和完整日志
的路径。常见原因：

- CRIU 摘要中出现 `Can't handle non-regular mapping` 或 `handle_device_vma plugin failed`：CUDA
  进程未经 `exec` fork 出的子进程仍映射着 `/dev/nvidia*`。fork guard 通常可以防止这种情况。请
  检查 `fork_guard` 是否不为 `false`、程序是否不是静态链接的，以及作业是否在 fork guard 安装之后
  启动。否则请使用 `spawn` 或 `forkserver` 启动方式（PyTorch DataLoader 使用
  `multiprocessing_context="forkserver"`）。
- 在 `checkpoint_tcp_close = false` 时，CRIU 会拒绝转储有已建立 TCP 连接的作业。

在操作系统关机或重启期间，转储失败或被拒绝的作业会被 systemd 停止。启动后，它会显示为
`Checkpoint failed` 并附带原因（拒绝的原因以 `checkpoint
refused:` 开头），其所有者会收到警告。

<a id="restore-failures"></a>
### 恢复失败

恢复失败时，作业保持 `Checkpointed` 状态并保留其镜像，因此你可以修复原因后再次运行
`puzuz restore`。所有者会在下次执行命令时收到警告。

- `puzuz refuses to run jobs as <user>: ...`：作业的所有者或组被 `job_min_uid`、`job_denied_groups`
  或 `job_allowed_users` 拒绝（见 [谁可以运行作业](#who-jobs-may-run-as)）；`the checkpointed job ran with
  the groups ["docker"], which are in job_denied_groups` 表示作业在带有现在被拒绝的组时创建了检查点。
  请更改设置（并重启 helper）或删除该作业。

- `the checkpoint of job N was dumped but not finalized, and finalizing it failed again: ...`：转储
  已成功，但 helper 无法写入检查点的元数据（见 [检查点存储](#checkpoint-storage)），通常是因为
  `checkpoints_dir` 所在的磁盘已满或出现故障。修复后再次恢复。

- `paths the checkpoint's mounts depend on are missing on this host: [...]`：CRIU 会以
  `Can't stat mountpoint` 或 `Failed to open_tree <path>` 失败。作业的命名空间中包含一个现在不
  存在的挂载点，或来自某个现在不存在的主机路径的绑定挂载（例如重启后未挂载的文件系统）。
  请挂载它或创建该路径，然后再次恢复。
- `filesystems the checkpoint depends on are not mounted as at the dump (mount them and restore
  again): /data needs ext4 on /data (found overlay on /)`：作业启动时已挂载的某个文件系统现在没有
  挂载，或者那里挂载了另一种类型的文件系统。CRIU 会改为绑定空的挂载点目录，作业将继续写入
  下层的文件系统。请挂载它（`systemctl start data.mount`、`mount -a`、启动 NFS 服务器……），然后
  再次恢复。见 [文件系统和挂载](#filesystems-and-mounts)。
- `files or directories the job used are missing on this host: [...]`：作业打开的文件或其工作目录
  已不存在，通常是因为它们位于重启后变空的 tmpfs（`/scratch`）上。该作业无法恢复。请删除它
  （`puzuz remove`）。
- `checkpoint is not compatible with this host`：GPU 型号或驱动版本不同，或缺少 GPU。请使用原来的
  驱动和 GPU 恢复。在插件模式下 GPU UUID 也必须一致。在手动模式下，同型号的另一张卡会被
  重新映射。
- `not enough free GPU memory`：其他作业正在使用该 GPU。请等待或释放它。
- 进程打开的文件，或其可执行文件和库，被修改或删除（见 `restore.log`）。请在相同路径恢复它们。
