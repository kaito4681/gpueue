<a id="gpueue"></a>
# gpueue

[![CI](https://github.com/kaito4681/gpueue/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/kaito4681/gpueue/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/kaito4681/gpueue)](https://github.com/kaito4681/gpueue/releases/latest)
[![License: MIT](https://img.shields.io/github/license/kaito4681/gpueue)](LICENSE)

[English](README.md) | 日本語 | [简体中文](README.zh-CN.md)

複数のユーザーで共有する 1 台の Linux マシン向けの GPU ジョブキューです。CLI は
[pueue](https://github.com/Nukesor/pueue) と同じように使えます。gpueue はその上に 2 つの機能を加えます。GPU
スロットと、[CRIU](https://criu.org) と NVIDIA [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint)
による実行中ジョブの透過的なチェックポイント/リストア（CPU、メモリ、GPU の状態）です。マシンの再起動時にまだ
動いている学習はディスクに保存され、止まったところから再開します。最初からやり直すことはありません。

> [!NOTE]
> **状態：プレリリース。** 以下の機能はすべて実装済みで、Docker 内（PID 1 の systemd、本物の CRIU、本物の
> NVIDIA GPU）と、CPU ジョブを動かした QEMU/KVM 仮想マシンの実際の再起動と電源断をまたいでテストされています。
> 物理マシン（ベアメタル）での実際の再起動や、GPU ジョブを動かしての実際の再起動をまたいだテストは**まだ**
> 行っていません。
> [プロジェクトの状況](#project-status) を参照してください。

<a id="contents"></a>
## 目次

- [なぜ gpueue か](#why)
- [機能](#features)
- [インストール](#install)
  - [1. 前提条件](#1-prerequisites)
  - [2. gpueue のインストール](#2-install-gpueue)
  - [3. インストールの確認](#3-check-the-installation)
- [使い方](#usage)
- [制限事項](#limitations)
- [プロジェクトの状況](#project-status)
- [コーディングエージェント向け Agent Skill](#agent-skill)
- [ドキュメント](#documentation)
- [コントリビューション](#contributing)
- [ライセンス](#license)

<a id="why"></a>
## なぜ gpueue か

共有の GPU ワークステーションでは、ジョブを `nohup` や `tmux`、ユーザーごとの pueue で動かし、GPU の取り合いを
チャットで調整するのが普通です。カーネルやドライバーの更新のための再起動は、すべての学習を止めてしまいます。
gpueue は全員に 1 つのキューを提供します。システム全体のデーモンが全ユーザーのジョブをスケジュールし、誰がどの
GPU を使っているかを表示します。ジョブは特定の GPU を要求し（`--gpu 0,1`）、それらが空いたときにだけ開始します。
実行中のジョブは、OS のシャットダウン時、`gpueue shutdown` の実行時、または `gpueue checkpoint` で必要なときに
保存され、あとで `gpueue restore` で再開できます。コードを変更する必要はありません。

<a id="features"></a>
## 機能

| 機能 | 内容 |
| --- | --- |
| pueue 互換の CLI | `add`、`status`、`log`、`follow`、`wait`、`pause`、`start`、`kill`、`remove`、`restart`、`clean`、`reset`、`stash`、`enqueue`、`switch`、`group`、`parallel`、`shutdown`、`completions`。加えて、グループ、依存関係（`--after`）、優先度、遅延実行、ラベル |
| GPU スロット | GPU ごとに同時に実行できるジョブ数の上限（`gpueue gpu parallel`）。各ジョブには `CUDA_VISIBLE_DEVICES` と `CUDA_DEVICE_ORDER=PCI_BUS_ID` が設定されます。GPU は NVML で検出します |
| 公平な共有 | Slurm 式のマルチファクター優先度（減衰する使用量によるフェアシェア、待ち時間、nice、`top`）。`gpueue prio` と `gpueue share` で数値を確認できます |
| チェックポイント/リストア | CPU と CUDA プロセスの透過的なチェックポイント/リストア。DataLoader のワーカーを fork する PyTorch のジョブも含みます（[fork ガード](docs/ja/usage.md#limitations) を参照）。OS のシャットダウンや再起動のときは、実行中の全ジョブを自動でチェックポイントします |
| マルチユーザー | ジョブはその所有者として実行されます。他のユーザーはジョブが存在することは分かりますが、そのコマンド、環境変数、ログは見えません。管理者は root と `gpueue-admin` グループです |
| 権限分離 | デーモン（HTTP API、スケジューラー、SQLite）は特権のないシステムユーザー `gpueue` として動きます。ジョブの起動と CRIU の実行は、小さな root ヘルパーだけが行います |
| 止まらないジョブ | ジョブはそれぞれ専用の systemd ユニットで動くため、デーモンやヘルパーを再起動しても（たとえばアップグレード中でも）動き続けます |
| `gpueue doctor` | インストール状態を確認し、問題ごとにそれを直すコマンドを表示します |

<a id="install"></a>
## インストール

gpueue は管理者がマシンごとに 1 回インストールします。一般ユーザーは何もインストールする必要はありません。
各ステップの詳細は [docs/ja/install.md](docs/ja/install.md) にあります。

<a id="1-prerequisites"></a>
### 1. 前提条件

gpueue には、systemd 240 以降の Linux と、`CONFIG_CHECKPOINT_RESTORE` を有効にしたカーネル
（5.5 以降を推奨）が必要です。リリースは x86_64 と aarch64 向けにビルドされ、glibc 2.34 以降
（Ubuntu 22.04 以降、Debian 12 以降、RHEL 9 以降）が必要です。実機でテストしたのは x86_64 だけです。

下のプログラムがなくてもジョブは実行できますが、チェックポイントには必要です。どのインストール方法もこれらは
インストールしません。パッケージは CRIU を推奨するだけで、多くのリリースではその CRIU は古いか存在せず、
`install.sh` は足りないものを表示するだけです
（[各方法が何をインストールするか](docs/ja/install.md#dependencies-that-are-not-installed-automatically)を参照）。
NixOS モジュールは独自の CRIU を持ち込みます。`.deb` と `.rpm` は、CRIU がないか 4.2 より古いとインストール時に
警告し、`gpueue doctor` がディストリビューションに合ったインストール方法を示します。

<a id="dependencies-that-are-not-installed-automatically"></a>

| 依存関係 | 必要になる場面 | 入手方法 |
| --- | --- | --- |
| CRIU 4.2 以降 | すべてのチェックポイント | `sudo apt install criu`（Ubuntu 26.04）、`sudo dnf install criu`（Fedora）、`sudo pacman -S criu`（Arch Linux）。それ以外では[ソースからビルド](docs/ja/install.md#criu)します |
| NVIDIA ドライバー r570 以降と `nvidia-smi` | GPU ジョブ | ディストリビューションから。その `cuda-checkpoint` が `--action lock/checkpoint/restore/unlock` に対応している必要があります |
| ドライバーと同じメジャーバージョンの NVIDIA のビルド済み cuda-checkpoint（root の所有） | GPU のチェックポイント | 下のコマンド（NixOS では [NixOS での cuda-checkpoint](docs/ja/install.md#cuda-checkpoint-on-nixos) を参照） |

```sh
git clone --depth 1 https://github.com/NVIDIA/cuda-checkpoint /tmp/cuda-checkpoint
sudo install -o root -g root -m 0755 "/tmp/cuda-checkpoint/bin/$(uname -m)_Linux/cuda-checkpoint" \
  /usr/local/bin/cuda-checkpoint
```

ヘルパーは cuda-checkpoint と CRIU を起動時にだけ探します。gpueue の後にどちらかをインストールしたら
`sudo systemctl restart gpueue-helper` でヘルパーを再起動し（[cuda-checkpoint](docs/ja/install.md#cuda-checkpoint)
を参照）、もう一度 `gpueue doctor` を実行してください。

<a id="2-install-gpueue"></a>
### 2. gpueue のインストール

インストーラーは最新のリリースをダウンロードして検証し、gpueue を /usr/local/bin にインストールします。
systemd ユニット、`gpueue` ユーザー、`gpueue-admin` グループ、ディレクトリ、デフォルトの
/etc/gpueue/config.toml を用意し、サービスを起動して `gpueue doctor` のレポートを表示します。

```sh
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

あとで `sudo gpueue update` で更新できます（実行中のジョブは動き続けます）。削除は `sudo gpueue uninstall` で
行い、データは残ります（`--purge` を付けるとデータも削除します）。GitHub のトークンは不要です（`GH_TOKEN` が
設定されていれば使いますが、API のレート制限が上がるだけです）。

<details>
<summary><b>ほかの方法：APT / .deb、.rpm、NixOS と Nix、Arch Linux</b></summary>

Debian と Ubuntu（amd64 または arm64）では、代わりに gpueue の APT リポジトリ（GitHub リリースの `apt`）
からパッケージをインストールできます。`curl` と `ca-certificates` が必要で、最新の安定版とアーキテクチャは
apt が選びます。

```sh
sudo curl -fsSLo /etc/apt/keyrings/gpueue.gpg https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/gpueue.sources https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.sources
sudo apt update && sudo apt install gpueue
```

または、最新リリースの `.deb` を直接インストールします（同じリポジトリが登録されます）。

```sh
arch=$(dpkg --print-architecture)
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb"
sudo apt install "./gpueue_${arch}.deb"
```

同じサービスをインストールして起動し、`criu` を推奨パッケージとします（前提条件は
[docs/ja/install.md](docs/ja/install.md#installing-the-debianubuntu-package) を参照）。どちらの方法でも、
以降のバージョンは `sudo apt update && sudo apt upgrade` で入ります。`sudo apt remove gpueue` はサービスを
停止し、`sudo apt purge gpueue` は /etc/gpueue も削除します。どちらも /var/lib/gpueue と実行中のジョブは
残します。

Fedora、RHEL、Rocky、Alma Linux 9 以降（x86_64 または aarch64）では、リリースの .rpm をインストール
します（dnf は URL をそのまま受け付けます）。

```sh
sudo dnf install "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue.$(uname -m).rpm"
```

これらの URL は常に最新の安定版リリースを指します（`releases/latest` はプレリリースを飛ばします）。特定の
バージョン用に、バージョン付きのファイル（`gpueue_<version>_<arch>.deb`、`gpueue-<version>-1.<arch>.rpm`）
も添付されています。どのファイルにも `.sha256` と `.minisig` があります
（[リリースを手動で検証する](docs/ja/install.md#verifying-a-release-by-hand) を参照）。

dnf リポジトリはまだありません（予定、issue #23）。更新するには、同じコマンドを `sudo dnf upgrade`
で実行します。`sudo dnf remove gpueue` はサービスを停止し、
/var/lib/gpueue と実行中のジョブは残します。`criu` を推奨パッケージとします。Fedora のものは十分に新しい
ですが、Rocky/RHEL 9 には 3.19 しかないため、チェックポイントにはより新しい CRIU が必要です（CRIU が
なくてもジョブは実行できます）。ソースからビルドしたり、Nix flake を使ったりもできます
（[docs/ja/install.md](docs/ja/install.md) を参照）。

NixOS では、flake のモジュール（`github:kaito4681/gpueue/stable`、`nixosModules.default`）をインポートし、
`services.gpueue.enable = true` を設定します（[NixOS](docs/ja/install.md#nixos) を参照）。home-manager の
ユーザーは `homeManagerModules.default` で CLI の補完を追加できます。他のディストリビューション上の Nix
（x86_64 または aarch64）では、flake をビルドし、その結果からサービスをインストールします（バイナリは
/usr/local/bin にコピーされ、Nix のガベージコレクションから保護されます。
[Nix でのインストール](docs/ja/install.md#installing-with-nix) を参照）。

```sh
nix build github:kaito4681/gpueue/stable
sudo ./result/bin/gpueue service install
```

Arch Linux（x86_64、aarch64）では、当面は上記のインストーラーを使ってください。AUR パッケージ `gpueue-bin`
は公開予定で、まだ公開されていません（[#159](https://github.com/kaito4681/gpueue/issues/159)）。CRIU（チェックポイントには 4.2 以降）は公式リポジトリにあります。

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

`sudo gpueue update` で更新し、`sudo gpueue uninstall` で削除します。パッケージが AUR に公開されたら、
代わりに任意の AUR ヘルパーで [`gpueue-bin`](https://aur.archlinux.org/packages/gpueue-bin) を
インストールできます（[docs/ja/install.md](docs/ja/install.md#arch-linux-aur) を参照）。

</details>

<a id="3-check-the-installation"></a>
### 3. インストールの確認

```sh
# Check everything: criu, cuda-checkpoint, driver, kernel, systemd, services, storage.
gpueue doctor

# Optional: make someone an administrator.
sudo usermod -aG gpueue-admin alice
```

<a id="usage"></a>
## 使い方

`gpueue --help` がすべてのコマンドを一覧表示します。`(gpueue)` の付いていないコマンドは pueue と同じように
動作し（[違い](docs/ja/usage.md#pueue-compatibility-and-differences)は主に、共有の 1 つのデーモンと、誰が何を
できるかに関するものです）、付いているものは gpueue で追加したものです。各コマンドのオプション（たとえば `add`
の `--gpu` と `--no-checkpoint`）は `gpueue <command> --help` で表示されます。

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

  -j, --json
          Print JSON: alone, the job list as `status --json` does; before a command that has
          `--json`, the same as giving it to that command

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

典型的な使い方：`add` は指定した GPU でジョブをキューに入れ、`status` と `gpu` は誰がどこで何を実行しているかを、
`log` と `follow` は出力を表示します。`checkpoint` / `restore` はジョブを GPU から降ろし、あとで戻します。

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

リストアされたジョブは、チェックポイントされた瞬間から処理を続けます。ジョブ ID は変わらず、出力は同じログに
追記されます。OS がシャットダウンするとき、ヘルパーは実行中のジョブを 1 つずつチェックポイントします
（`--no-checkpoint` のジョブは単に終了されます）。起動後それらは `Checkpointed` になり、各所有者が
`gpueue restore <id>` か `gpueue restore -a` で再開します。リストアが自動で始まることはありません。
`start`、`add -i`、`restart -i`、`restore -i` でジョブをすぐに開始できるのは管理者だけで、それ以外のユーザーの
ジョブは自分のジョブの先頭にキューされます。

それ以外のリファレンスは [docs/ja/usage.md](docs/ja/usage.md) にあります：
[pueue との違いと追加のコマンド](docs/ja/usage.md#pueue-compatibility-and-differences)、
[JSON 出力](docs/ja/usage.md#json-output)、
[一時停止中のジョブ、再起動、失敗したチェックポイント](docs/ja/usage.md#checkpoint-and-restore)、
[誰が何をできるか](docs/ja/usage.md#multi-user-model-and-authorization)、
[GPU スロットと分離](docs/ja/usage.md#gpu-slots)、
[ジョブの優先度](docs/ja/usage.md#job-priority)。

<a id="limitations"></a>
## 制限事項

チェックポイント/リストアには制限があります。確立済みの TCP 接続はチェックポイント時に閉じられ、電源断や
カーネルのクラッシュでは何も保存されません。ジョブをリストアできるのは、同じ GPU モデルとドライバーで、同じ
ファイルとマウントがある場合だけです。ある種のソケットやディスクリプタ（ジョブの `/tmp` の外の Unix ソケット、
inotify、`SO_REUSEPORT` など）を持つジョブはチェックポイントできません。その場合チェックポイントは拒否され、
ジョブは動き続けます。[すべての制限事項](docs/ja/usage.md#limitations)を参照してください。

<a id="project-status"></a>
## プロジェクトの状況

gpueue は 1.0 より前の、活発に開発中のプロジェクトです。現在のバージョンとリリースノート（セキュリティ修正の
告知も含みます）は[リリースページ](https://github.com/kaito4681/gpueue/releases)にあります。ユニットテスト、
統合テスト、フルスタックのエンドツーエンドテストを Docker 内の systemd のもとで（本物の GPU でも）実行し、実際の
再起動、電源断、アップグレードを QEMU/KVM 仮想マシンで実行しています（ディストリビューションは
`scripts/vm-reboot-test.sh list` で表示されます）。物理マシンでの実際の再起動と、実際の再起動の GPU に関わる部分は
まだテストしていません。詳しくは[テストの範囲](docs/ja/usage.md#test-coverage)を参照してください。お使いの
ハードウェアで何が動いて何が動かないか、ぜひ報告してください。

<details>
<summary><b>v0.1.4 からのアップグレード</b></summary>

v0.1.5 ではチェックポイント失敗時の扱いが変わり、CLI の機能がいくつか追加されました。次の動作の変更を
確認してください。

- CRIU とヘルパーがきれいにロールバックできたダンプの失敗では、ジョブは停止されなくなりました。ジョブは
  動き続け、最後のエラーは `checkpoint failed, the job keeps running:` で始まります。ロールバックが完全には
  成功しなかったダンプや、期限で CRIU が強制終了されたダンプでは、これまでどおりジョブは停止されます。
- ダンプの開始中にジョブが作る削除済みファイル（`/dev/shm` 上の PyTorch DataLoader の共有メモリなど）で
  ダンプが失敗しなくなりました。CRIU の ghost limit は空き容量になりました。
- ファイルロック（flock、POSIX、OFD のロック。たとえば `uv run` で起動したジョブ）を持つジョブを
  チェックポイントできるようになりました。ファイルの lease を持つジョブは拒否されます。
- `gpueue reset` は `-f` を付けない限り必ず確認するようになったため、スクリプトでは `-f` が必要です。
  チェックポイント済みのジョブを削除する前には別に確認し、`--keep-checkpoints` を付けるとそれらを残します。
  `gpueue remove` で「no」と答えると、チェックポイント済みのジョブを残して残りを削除します。
- `gpueue --json` はジョブ一覧を出力します。JSON 出力は端末ではインデントされます。
- `.deb` と `.rpm` は、CRIU がないか 4.2 より古い場合にインストール時に警告し、`gpueue doctor` は
  ディストリビューションに合った対処方法を示します。
- v0.1.4 で取ったチェックポイントをアップグレード後にリストアすることは、アップグレードのテスト
  （`scripts/vm-reboot-test.sh`、CPU ジョブ）で確認しています。

</details>

<details>
<summary><b>v0.1.3 以前からのアップグレード</b></summary>

v0.1.4 はセキュリティレビューで見つかった問題を修正しました
（[リリースノート](https://github.com/kaito4681/gpueue/releases/tag/v0.1.4)を参照）。v0.1.3 以前を使っているサイトは
アップグレードしてください。その前に次の動作の変更を確認してください。

- 次のジョブはチェックポイントできなくなりました（拒否のメッセージに理由が表示されます）：inotify/fanotify
  の監視を持つジョブ、（libcuda 自身のもの以外の）抽象 Unix ソケットで listen しているジョブ、Unix ソケットの
  キューにディスクリプタを持つジョブ、削除済みの setuid/setgid ファイルを持つジョブ。
- ジョブがバインドしていたポートをホスト上の別のプログラムが使っている間は、リストアが拒否されます。
  ポートが空いてから再試行できます。
- `/etc/gpueue/config.toml`（またはその上のディレクトリ）が root の所有でないか、グループやその他のユーザーが
  書き込める場合、`gpueue daemon` は起動を拒否します。`gpueue doctor` がこれを報告します。
- `sudo gpueue service install` は、root 以外のユーザーが書き込めるディレクトリからの `gpueue-helper` の
  コピーを拒否します。リリースは root として展開してください。
- v0.1.3 で取ったチェックポイントは v0.1.4 でリストアできます。

</details>

<a id="agent-skill"></a>
## コーディングエージェント向け Agent Skill

このリポジトリには、コーディングエージェント（Claude Code、Codex、Cursor など）に一般ユーザーとしての
gpueue の使い方を教える [Agent Skill](https://agentskills.io) が [skills/gpueue](skills/gpueue/SKILL.md)
に含まれています。内容は、デーモンが動いているかの確認、作業ディレクトリ・環境変数・`--gpu` を正しく
指定したジョブの追加、ログの追跡、ポーリングの代わりに `gpueue wait` で待つこと、pause/kill/restart、
checkpoint/restore、`--json` 出力、よくあるエラー、安全のためのルール（自分のジョブだけを操作し、`sudo`
や管理者コマンドは使わない）です。[skills](https://github.com/vercel-labs/skills) CLI でインストールします。

```sh
npx skills add kaito4681/gpueue              # このプロジェクトのみ。全プロジェクトなら -g
npx skills add kaito4681/gpueue -g -a claude-code -y
```

<a id="documentation"></a>
## ドキュメント

| ドキュメント | 内容 |
| --- | --- |
| [docs/ja/install.md](docs/ja/install.md) | 要件、ビルド、インストール、設定、優先度とフェアシェア、アップグレード、アンインストール、トラブルシューティング |
| [docs/ja/usage.md](docs/ja/usage.md) | pueue との違い、JSON 出力、チェックポイント/リストア、マルチユーザーモデル、GPU スロット、ジョブの優先度、制限事項、テストの範囲 |
| [docs/ja/architecture.md](docs/ja/architecture.md) | コンポーネント、ジョブの流れ、シャットダウンの順序、状態の保存、クレートの構成 |

<a id="contributing"></a>
## コントリビューション

ワークスペースの構成と規約については [AGENTS.md](AGENTS.md) を読んでください。コード、コメント、
ドキュメント、コミットメッセージは英語で書きます。テストは Docker 内で実行します。

```sh
scripts/test-in-docker.sh                                   # cargo test --workspace
scripts/test-in-docker.sh clippy --workspace --all-targets
scripts/test-in-docker.sh fmt --all
```

<details>
<summary><b>エンドツーエンド、VM、GPU、Nix のテスト</b></summary>

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

`scripts/e2e-test.sh` は冒頭で環境変数を説明しています。たとえば `GPUEUE_E2E_ONLY` や、GPU のシナリオ用の
`GPUEUE_E2E_GPU=1` です。`GPUEUE_HELPER_GPU=1 scripts/helper-systemd-test.sh`（CUDA のベースイメージが必要。
スクリプトを参照）は、どちらの `GPUEUE_HELPER_CUDA_MODE` でも、GPU のチェックポイントを期限切れに追い込み、
ダンプの途中で `criu` / `cuda-checkpoint` を kill します。`scripts/measure-checkpoint.sh` はチェックポイントの
各ステップにかかる時間を測定します。どのスクリプトもホスト上で特権の必要な処理は実行しません。

</details>

<a id="license"></a>
## ライセンス

[MIT](LICENSE)
