<a id="installing-and-operating-gpueue"></a>
# gpueue のインストールと運用

[English](../install.md) | 日本語 | [简体中文](../zh-CN/install.md)

<!--
Anchor convention for translations (docs/ja/, docs/zh-CN/, README.ja.md, README.zh-CN.md):
every heading is preceded by an explicit anchor whose id is the GitHub slug of the matching
English heading (including the "-1" suffix GitHub adds to duplicates), e.g.

  <a id="install-method"></a>
  ### インストール方法

Links within and between the translated pages use these English ids and point to the page of
the same language (README.ja.md -> docs/ja/install.md#criu). Links to repository files from
docs/<lang>/ go up two levels (../../packaging/...). Code blocks are copied unchanged.
-->

このガイドはマシンの管理者向けです。一般ユーザーに必要なのは、`PATH` 上の `gpueue` コマンドだけです。

- [要件](#requirements)
- [install.sh でのインストール](#installing-with-installsh)
- [Debian/Ubuntu パッケージのインストール](#installing-the-debianubuntu-package)
- [Fedora/RHEL/Rocky パッケージのインストール](#installing-the-fedorarhelrocky-package)
- [Nix でのインストール](#installing-with-nix)
- [Arch Linux (AUR)](#arch-linux-aur)
- [ビルド](#building)
- [インストール](#installing)
- [確認](#verifying)
- [設定](#configuration)
- [管理者](#administrators)
- [ジョブを実行できるユーザー](#who-jobs-may-run-as)
- [ジョブにできること・できないこと](#what-a-job-can-and-cannot-do)
- [優先度とフェアシェア](#priority-and-fair-share)
- [GPU の検出](#gpu-detection)
- [GPU の分離](#gpu-isolation)
- [チェックポイントの保存先](#checkpoint-storage)
- [CUDA プラグインモード](#cuda-plugin-mode)
- [ファイルシステムとマウント](#filesystems-and-mounts)
- [アップグレード](#upgrading)
- [アンインストール](#uninstalling)
- [トラブルシューティング](#troubleshooting)

<a id="requirements"></a>
## 要件

| コンポーネント | 要件 | テスト環境 |
| --- | --- | --- |
| OS | init が systemd の Linux、x86_64 または aarch64（リリースは両方向けにビルド） | Ubuntu（PID 1 が systemd の Docker イメージ）、x86_64 |
| systemd | 240 以降（`gpueue doctor` が確認します） | 259 |
| カーネル | `CONFIG_CHECKPOINT_RESTORE=y`（`/proc/sys/kernel/ns_last_pid` が存在すること）、PID 名前空間とマウント名前空間。5.5 以降を推奨（`set_tid` 付きの `clone3`。CRIU が PID のリストアに使用）、`open_tree`/`move_mount` には 5.2 以降（隠れたマウントの整理に使用）、リストアには 5.8 以降（`openat2`、`faccessat2`、`statx` のマウント ID。リストアが開くものの確認に使用） | 7.0 |
| CRIU | 4.2 以降 | 4.2（ディストリビューションのパッケージ）、4.2.1 |
| NVIDIA ドライバー（GPU ジョブ） | `cuda-checkpoint` が `--action lock/checkpoint/restore/unlock` と `--timeout` に対応したドライバー：r570 以降（cuda-checkpoint 自体は r550 に対応していますが、`--toggle` のみで、gpueue はこれを使いません） | 595 |
| cuda-checkpoint（GPU ジョブ） | NVIDIA のバイナリ。メジャーバージョンがドライバーと同じもの | 595 |
| nvidia-smi（GPU ジョブ） | ヘルパーが CUDA プロセスと GPU メモリを調べるのに使います | ドライバー付属 |
| Rust（ビルド時のみ） | 1.88 以降と、ヘルパーの fork ガードライブラリ用の C コンパイラー（`cc`） | 1.98.1 |

NVIDIA ドライバーがなくても、gpueue は CPU のみのジョブを実行でき、それらのチェックポイントも取れます。

<a id="criu"></a>
### CRIU

チェックポイントには CRIU 4.2 以降が必要です。CRIU がなくてもジョブは実行できます。`gpueue doctor` は CRIU が
ない場合や古い場合に報告します。ヘルパーは起動時に一度だけ CRIU を確認し、CRIU がない場合や古い場合は、ジョブに
触れる前にすべてのチェックポイントを拒否します。ジョブは動き続けます（シャットダウン時には、チェックポイントを
拒否された他のジョブと同じく停止されます）。CRIU をインストールまたは更新したらヘルパーを再起動してください
（`sudo systemctl restart gpueue-helper`）。ディストリビューションのパッケージが十分に新しいのは Ubuntu 26.04
（`sudo apt install criu`、universe）、Fedora（`sudo dnf install criu`、4.2）、Arch
（`sudo pacman -S criu`）です。Ubuntu 22.04（3.16）、Ubuntu 24.04（パッケージなし）、Debian 12（3.17）、
Debian 13（4.1）、RHEL/Rocky/Alma 9（3.19）では足りません。<https://criu.org/Installation> に従って CRIU を
ソースからビルドする（上流のリリース tarball を `make` し、`sudo make install` で `/usr/local/sbin` に
インストール）か、`/etc/gpueue/config.toml` の `criu_path` で root が所有するビルドを指定してください。
`.deb` と `.rpm` は `criu` を推奨するだけなので、これらのリリースでは apt や dnf は古いパッケージを
インストールするか、何もインストールしません。ヘルパーの `PATH` では `/usr/local/sbin` の CRIU が優先されます。
デフォルトの **manual** モード（`cuda_checkpoint_mode = "manual"`）では、ヘルパーが自分で cuda-checkpoint を
呼び出し、CRIU をプラグインなしで実行します。ディストリビューションのパッケージは CRIU の CUDA プラグインを
含んでいませんが、この用途には使えます。

**plugin** モード（`cuda_checkpoint_mode = "plugin"`）では、代わりに CRIU の CUDA プラグイン
（`cuda_plugin.so`、CRIU 4.0 以降）が cuda-checkpoint を操作します。これは任意です。
[CUDA プラグインモード](#cuda-plugin-mode) を参照してください。

<a id="cuda-checkpoint"></a>
### cuda-checkpoint

NVIDIA は cuda-checkpoint をビルド済みのバイナリとして <https://github.com/NVIDIA/cuda-checkpoint> の
`bin/x86_64_Linux/cuda-checkpoint`（arm64 では `bin/aarch64_Linux/`）で配布しています。メジャーバージョンは
ドライバーと同じでなければなりません（違うと `gpueue doctor` が警告します）。root の `PATH` で見つかる場所に、
root の所有で、他のユーザーが書き込めない状態で置いてください（`uname -m` は `x86_64` か `aarch64` を表示し、
これでディレクトリが決まります）。

```sh
git clone --depth 1 https://github.com/NVIDIA/cuda-checkpoint /tmp/cuda-checkpoint
sudo install -o root -g root -m 0755 "/tmp/cuda-checkpoint/bin/$(uname -m)_Linux/cuda-checkpoint" \
  /usr/local/bin/cuda-checkpoint
```

別の場所に置いて、`/etc/gpueue/config.toml` で `cuda_checkpoint_path` を設定することもできます。ヘルパーは
`criu`、`cuda-checkpoint`、`nvidia-smi` を root として実行するので、それらの絶対パスは root の所有でなければ
なりません。root が所有するシンボリックリンクはたどられ、リンク先も同じように確認されます。また
`/nix/store`（root の所有、sticky、グループ `nixbld`）はグループ書き込み可能ですが受け入れられるので、NixOS の
`/run/current-system/sw/bin/criu` のようなパスも使えます。`gpueue doctor` も、root かどうかにかかわらず
（確認はメタデータを読むだけです）、これらのプログラムを実行する前に同じ規則を適用します。規則を満たさない
プログラムは実行せずに FAIL として報告します。パスを含まない名前はヘルパーと同じように systemd のデフォルトの
`PATH` から探し、実行ビットのないファイルは飛ばします。root として実行したときは、設定ファイル自体が所有者の
確認に失敗した場合、そのファイルが指定するプログラムを一つも実行しません。

ヘルパーは名前で指定されたプログラムを、起動時に一度だけ探します。**cuda-checkpoint（または CRIU）を
インストールしたらヘルパーを再起動してください**（`sudo systemctl restart gpueue-helper`）。それまでは
ヘルパーはそれらを使えず、GPU ジョブのチェックポイントを拒否し（`finding CUDA processes: the helper did not
find cuda-checkpoint when it started ...`）、`gpueue doctor` はそれを報告します（`helper programs`）。
同じように、ヘルパーが起動時に見つけたプログラムがその後削除・移動された場合も、再起動するまでヘルパーは
それを使えません。

<a id="dependencies-that-are-not-installed-automatically"></a>
### 自動ではインストールされない依存関係

どのインストール方法も NVIDIA ドライバーと cuda-checkpoint はインストールしません。どこでも十分に新しい
CRIU を持ち込むのは NixOS モジュールだけです。gpueue 本体のほかに各方法がインストールするもの：

| インストール方法 | インストールするもの | 自分で用意するもの |
| --- | --- | --- |
| [install.sh](#installing-with-installsh)、[Nix ビルド](#other-distributions) + `gpueue service install` | ほかには何もなし（install.sh は CRIU と cuda-checkpoint がなければその入れ方を表示） | CRIU、NVIDIA ドライバー、cuda-checkpoint |
| [.deb](#installing-the-debianubuntu-package) | 依存パッケージ（libc6、libgcc-s1、systemd 240 以降）と、リリースにあれば推奨の `criu` | リリースの CRIU がないか古い場合は CRIU 4.2 以降、NVIDIA ドライバー、cuda-checkpoint |
| [.rpm](#installing-the-fedorarhelrocky-package) | 依存パッケージ（glibc 2.34 以降、libgcc、systemd）と推奨の `criu`（弱い依存） | RHEL/Rocky/Alma 9（3.19）では CRIU 4.2 以降、NVIDIA ドライバー、cuda-checkpoint |
| [AUR](#arch-linux-aur) | 依存パッケージ（systemd、glibc、libgcc）。`criu` と `cuda-checkpoint` は任意の依存にすぎない | CRIU、NVIDIA ドライバー、cuda-checkpoint |
| [NixOS モジュール](#nixos) | CRIU 4.2.1。`nvidia-smi` と NVML は `hardware.nvidia.package` から | NVIDIA ドライバー（`hardware.nvidia`）、cuda-checkpoint（[`cudaCheckpointPackage`](#cuda-checkpoint-on-nixos)） |

CRIU と cuda-checkpoint がなくてもジョブは実行できますが、チェックポイントには必要です。追加するには
（NixOS 以外）：

```sh
# CRIU 4.2 or newer, where the distribution has it (otherwise build it, see CRIU above)
sudo apt install criu       # Ubuntu 26.04
sudo dnf install criu       # Fedora
sudo pacman -S criu         # Arch Linux

# For GPU jobs, with an NVIDIA driver r570 or newer installed: cuda-checkpoint
git clone --depth 1 https://github.com/NVIDIA/cuda-checkpoint /tmp/cuda-checkpoint
sudo install -o root -g root -m 0755 "/tmp/cuda-checkpoint/bin/$(uname -m)_Linux/cuda-checkpoint" \
  /usr/local/bin/cuda-checkpoint

# The helper looks the programs up only when it starts (see cuda-checkpoint above)
sudo systemctl restart gpueue-helper
gpueue doctor
```

<a id="installing-with-installsh"></a>
## install.sh でのインストール

`scripts/install.sh` は、x86_64 と aarch64 の Linux に gpueue をインストールする通常の方法です（Debian と
Ubuntu では [パッケージ](#installing-the-debianubuntu-package) も選べます）。すべてのリリースに添付されて
います。リリースの tarball をダウンロードして検証し、そのリリースの `gpueue service install` を実行します。
これはバイナリを `/usr/local/bin` にコピーします（何をするかは [インストール](#installing) を参照）。

<a id="github-token-optional"></a>
### GitHub のトークン（任意）

ダウンロードに GitHub のトークンは不要です。トークンは、匿名リクエストに対する GitHub API のレート制限
（IP アドレスごとに 1 時間あたり 60 回。共有の NAT では使い切ることがあります）を上げるだけです。どんな
トークンでも使えます。fine-grained のトークンには権限は必要ありません。`install.sh` は次の順序でトークンを
探し、見つかれば使います。

1. 環境変数 `GH_TOKEN` または `GITHUB_TOKEN`
2. `--token-file FILE`（FILE の 1 行目）
3. `sudo` を実行したユーザーとして実行する `gh auth token`（GitHub CLI がインストールされ、ログイン済みの場合）
4. `--save-token` が書き込む `/etc/gpueue/github-token`（root の所有、モード 0600）

トークンはヘッダーファイルで `curl` に渡されるので、`ps` や出力には現れません。トークンが拒否された場合は
そう伝えるメッセージで失敗します。`--version X` で 404 になった場合は、そのリリースが存在しないという
意味です。

<a id="running-the-installer"></a>
### インストーラーの実行

```sh
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

スクリプトをパイプで渡す場合、オプションは `bash -s --` のあとに書きます（たとえば
`... | sudo bash -s -- --version 0.2.0`）。ダウンロードしたファイルを使う場合はファイル名のあとに書きます
（`sudo bash install.sh --version 0.2.0`。root で実行する前に署名を確認する方法は
[リリースを手動で検証する](#verifying-a-release-by-hand)を参照）。

| オプション | 意味 |
| --- | --- |
| `--version X` | 最新のリリースではなくバージョン X をインストールします |
| `--bin-dir DIR` | デフォルト（現在のインストール先のディレクトリ、なければ `/usr/local/bin`）以外の場所にインストールします |
| `--token-file FILE` | トークンを FILE から読み込みます |
| `--save-token` | `gpueue update` や以降の実行のために、トークンを `/etc/gpueue/github-token`（root、0600）に保存します |
| `--uninstall` | インストール済みの `gpueue uninstall` で gpueue を削除します（[アンインストール](#uninstalling) を参照） |
| `--purge` | `--uninstall` と一緒に使い、`/etc/gpueue` と `/var/lib/gpueue`（と `config.toml` が設定したデータディレクトリ）も削除します |
| `-y`、`--yes` | purge の前に確認しません |

トークンが保存されるのは `--save-token` を付けたときだけです。そうしておけば、期限切れや失効したトークンが、
どのトークンが使われたのか気づかないうちに以降の実行を失敗させることはありません。

`install.sh` は何かをダウンロードする前に、root として実行されていること、systemd が PID 1 であること、
マシンが glibc 2.34 以降の x86_64 または aarch64 であること、gpueue がパッケージマネージャーや Nix で
インストールされていないこと（[インストール方法](#install-method) を参照）を確認します。最後の確認では、
インストール済みの gpueue（ユニットが実行するもの、なければ `PATH` にあるもの）に
`gpueue service install-method` で問い合わせます。インストール済みの gpueue がなければ、明らかな場合
（`/etc/os-release` による NixOS、パッケージが所有するユニット、`/nix/store` にリンクされたユニット）だけを
自分で拒否し、残りはダウンロードしたバイナリが何かをインストールする前に判断します。その後、次のことを
行います。

1. リリースを調べます（`/repos/kaito4681/gpueue/releases/latest` または `/releases/tags/v<X>`）。
2. `gpueue-v<X>-<target>.tar.gz`（`x86_64-unknown-linux-gnu` または `aarch64-unknown-linux-gnu`）と
   その `.sha256` と `.minisig` ファイルを一時ディレクトリに（リダイレクト先も含めて HTTPS だけで）ダウンロードし、
   `.sha256` ファイルがその tarball の名前でのチェックサムであることを確かめてから、チェックサムを確認します。
3. スクリプトに組み込まれたリリース鍵（`packaging/minisign.pub`）で `.minisig` の署名を確認します。
   `PATH` にある `minisign` を使い、なければ GitHub から minisign 0.12 の静的バイナリのリリースを一時
   ディレクトリにダウンロードし、スクリプトに組み込まれた SHA-256 チェックサム（`scripts/dist/get-minisign.sh`
   と同じもの）で確認してから使います。このダウンロードに失敗すると、ディストリビューションから `minisign` を
   インストールして（例: `sudo apt install minisign`）再実行するよう伝えて止まります。署名の確認を省くことは
   ありません。署名の trusted comment は、要求したリリースと tarball の `gpueue <tag> <file>` でなければ
   なりません。これにより、古いリリースの（正しく署名された）tarball を新しい名前で配っても拒否されます。
4. tarball を展開し（リンクや tarball の外を指すパスは拒否します）、新しい `gpueue service install` を実行し
   （現在のインストール先のディレクトリを維持し、なければ `/usr/local/bin` を使います。`--bin-dir` は指定
   した場合にだけ渡されます）、その `gpueue doctor` のレポートを表示します。
5. CRIU と cuda-checkpoint がなければ、そのインストール方法を表示します。それらを自分でインストールする
   ことはありません。

`gpueue doctor` のチェックの一部だけが失敗し（たとえば CRIU がない）、サービスは動いている場合、
`install.sh` は警告付きで成功します。

以後は `sudo gpueue update` で更新します（[アップグレード](#upgrading) を参照）。`install.sh` を再度実行しても、
最新の（または指定した）バージョンがインストール済みのものの上にインストールされます。どちらの方法でも
実行中のジョブは動き続けます。

Debian と Ubuntu では代わりに [.deb](#installing-the-debianubuntu-package) を、Fedora、RHEL、Rocky、
Alma Linux では [.rpm](#installing-the-fedorarhelrocky-package) を、どこでも [Nix](#installing-with-nix) を、
Arch Linux では公開後に [AUR パッケージ](#arch-linux-aur) を使えます（それまでは上記のように `install.sh` を
使ってください）。以下で説明するようにソースからビルドすることもできます。

<a id="verifying-a-release-by-hand"></a>
### リリースを手動で検証する

各リリースには、ターゲット（`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`）ごとに
`.sha256` と `.minisig` ファイル付きの `gpueue-v<X>-<target>.tar.gz`、同じ 2 つのファイルがそれぞれに付いた
パッケージ `gpueue_<X>_amd64.deb`、`gpueue_<X>_arm64.deb`、`gpueue-<X>-1.x86_64.rpm`、
`gpueue-<X>-1.aarch64.rpm`、同じパッケージをバージョンなしの名前にしたもの（`gpueue_amd64.deb`、
`gpueue_arm64.deb`、`gpueue.x86_64.rpm`、`gpueue.aarch64.rpm`。`releases/latest/download/<name>` 用で、
それぞれに専用の 2 つのファイルが付きます）、`install.sh.minisig` 付きの `install.sh`、そして `.rpm` が
GPG 署名されているリリースでは `RPM-GPG-KEY-gpueue.minisig` 付きの `RPM-GPG-KEY-gpueue` があります。tarball（または
パッケージ）を自分で確認するには次のようにします。

```sh
sha256sum -c gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
minisign -Vm gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
```

minisign は署名の trusted comment（署名の対象に含まれます）を表示します。これはリリースとファイルを
示します（`Trusted comment: gpueue v0.1.0 gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz`）。ダウンロード
しようとしたリリースとファイルであることを確認してください。

`curl ... | sudo bash` はダウンロードした内容をそのまま実行します。先に `install.sh` を確認するには、署名と
一緒にダウンロードし、リリース鍵（`packaging/minisign.pub`）で検証してから、確認したファイルを実行します。

```sh
curl -fsSLO https://github.com/kaito4681/gpueue/releases/latest/download/install.sh
curl -fsSLO https://github.com/kaito4681/gpueue/releases/latest/download/install.sh.minisig
minisign -Vm install.sh -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
sudo bash install.sh
```

<a id="the-release-signing-key-maintainers"></a>
### リリースの署名鍵（メンテナー向け）

リリースは `.github/workflows/release.yml` で、リポジトリのシークレット `MINISIGN_SECRET_KEY` と
`MINISIGN_PASSWORD` に保存された minisign の秘密鍵で署名されます。公開鍵は `gpueue` に組み込まれる
`packaging/minisign.pub` と、`scripts/install.sh` の `MINISIGN_PUBKEY` です。使用中の鍵は本物のリリース鍵
（ID `EA3C2ED855877E21`）です。鍵をローテーションするとき（鍵の漏えいやメンテナーの交代）は、次のように新しい鍵
ペアを作り、公開鍵を 2 か所とも同じコミットで差し替え、2 つのシークレットを更新します。インストール済みの
`gpueue` は古い公開鍵を持っており、新しい鍵で署名されたリリースを検証できないので、`install.sh` での手動の
再インストールが必要です。

```sh
scripts/dist/gen-signing-key.sh        # asks for a password; writes the secret key under ~
gh secret set MINISIGN_SECRET_KEY -R kaito4681/gpueue < ~/gpueue-release-key-*/minisign.key
gh secret set MINISIGN_PASSWORD -R kaito4681/gpueue
git commit packaging/minisign.pub scripts/install.sh
```

秘密鍵とそのパスワードはバックアップしてください。新しい鍵で署名されたリリースを `gpueue update` が受け入れる
には、その前に `gpueue` の新しいリリースが必要です。リリースはドラフトとして作成されます。GitHub でドラフトを
公開してください。公開しないと、`install.sh`（公開済みの最新リリースを問い合わせる）からは見えません。
ドラフトが作成されるのは、タグのコミットで、そのリリース自身の `.deb` を使った VM テスト
（`scripts/vm-reboot-test.sh quick`、ジョブ `vm-test`）が通ったときだけです。このテストは Ubuntu 26.04 の VM に
apt でパッケージをインストールし、チェックポイント済み・実行中・一時停止中のジョブを抱えたまま再起動し、
前のリリースからアップグレードして、ジョブをリストアします。失敗したときは何も作成されません。原因を直して
新しいバージョンのタグを付けるか、失敗がリリースのせいでなければ失敗したジョブを再実行してください。

<a id="installing-the-debianubuntu-package"></a>
## Debian/Ubuntu パッケージのインストール

各リリースには、Debian 12 と Ubuntu 22.04、24.04、26.04 向けに、amd64 と arm64 の `.deb` があります。
安定版リリースは gpueue の APT リポジトリにも入っています。これは GitHub リリース `apt`
（`https://github.com/kaito4681/gpueue/releases/download/apt/`、`/etc/apt/keyrings/gpueue.gpg` の鍵で署名）にあるフラットリポジトリです。

**前提条件。**

- 以下のコマンドに必要な `curl` と `ca-certificates`（`sudo apt install curl ca-certificates`）。
  `/etc/apt/keyrings` はサポート対象のどのリリースでも apt に含まれています。
- PID 1 としての systemd 240 以降（パッケージが依存しています。systemd が PID 1 でなければサービスは有効化
  されるだけです。後述）。
- 任意：チェックポイント用の CRIU 4.2 以降（ディストリビューションのものはないか古いことが多いです。
  [CRIU](#criu) を参照）、GPU ジョブ用の NVIDIA ドライバーと cuda-checkpoint
  （[cuda-checkpoint](#cuda-checkpoint) を参照）。これらがなくてもジョブは実行できます。

**APT リポジトリから（推奨）。** リポジトリを追加してそこからインストールします。最新の安定版と
アーキテクチャは apt が選びます。ファイル名と内容は次のとおり正確にしてください（パッケージがインストール
するものと同じなので、あとでパッケージがそれらについて尋ねることはありません）。

```sh
sudo curl -fsSLo /etc/apt/keyrings/gpueue.gpg \
  https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/gpueue.sources \
  https://github.com/kaito4681/gpueue/releases/download/apt/gpueue.sources
sudo apt update && sudo apt install gpueue
```

**`.deb` を直接。** または、最新リリースからこのマシンのアーキテクチャのパッケージをダウンロードし、
apt でインストールします。apt は依存するものも一緒に入れ、パッケージは同じリポジトリを登録します。

```sh
arch=$(dpkg --print-architecture)
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb"
sudo apt install "./gpueue_${arch}.deb"
```

`releases/latest` は最新の安定版リリースです（プレリリースは飛ばされます）。`gpueue_<arch>.deb` はその
`gpueue_<version>_<arch>.deb` をバージョンなしの名前にしたコピーです。特定のバージョン用に、バージョン付きの
ファイルも添付されています（`https://github.com/kaito4681/gpueue/releases/download/v0.1.0/gpueue_0.1.0_amd64.deb`）。インストールする前に
ダウンロードしたファイルを検証するには次のようにします（[リリースを手動で検証する](#verifying-a-release-by-hand)
を参照）。

```sh
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb.sha256"
curl -fLO "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue_${arch}.deb.minisig"
sha256sum -c "gpueue_${arch}.deb.sha256"
minisign -Vm "gpueue_${arch}.deb" -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
```

trusted comment にはリリースとバージョンなしのファイル名が書かれています
（`Trusted comment: gpueue v0.1.0 gpueue_amd64.deb`）。

パッケージの内容：

| パス | |
| --- | --- |
| `/usr/bin/gpueue`、`/usr/bin/gpueue-helper` | バイナリ |
| `/usr/lib/systemd/system/gpueue.service`、`gpueue-helper.service` | ユニット（`ExecStart=/usr/bin/...`） |
| `/usr/lib/systemd/system/<target>.d/gpueue.conf` | `reboot.target`、`poweroff.target`、`halt.target`、`kexec.target`、`soft-reboot.target` の `JobTimeoutSec=` を延ばすドロップイン（[シャットダウンのタイムアウト](#shutdown-timeouts) を参照） |
| `/usr/lib/sysusers.d/gpueue.conf`、`/usr/lib/tmpfiles.d/gpueue.conf` | `gpueue` ユーザー、`gpueue-admin` グループ、ディレクトリ |
| `/etc/gpueue/config.toml` | デフォルトの設定（conffile：アップグレードしても編集内容は保持されます） |
| `/etc/apt/sources.list.d/gpueue.sources`、`/etc/apt/keyrings/gpueue.gpg` | gpueue の APT リポジトリとその署名鍵（conffile、後述） |
| `/usr/share/bash-completion`、`zsh/vendor-completions`、`fish/vendor_completions.d` | シェルの補完 |
| `/usr/share/doc/gpueue` | README、このガイド、アーキテクチャの説明、ライセンス |

初回のインストールでは、ユーザー、グループ、ディレクトリを作成し、`gpueue-helper.service` と
`gpueue.service` を有効化して起動します。結果は `gpueue doctor` で確認してください。systemd が PID 1 でない
場合（ビルド中のコンテナイメージ、chroot）は、サービスは有効化されるだけで、次の起動時に開始します。
`install.sh` と異なり、パッケージは設定に `[[gpus]]` のエントリーを追加しません。GPU は起動時に NVML が
見つけます（[GPU の検出](#gpu-detection) を参照）。

systemd 240 以降に依存し、`criu` は**推奨**するだけです。サポート対象のほとんどのリリースでは、
ディストリビューションの CRIU はないか 4.2 より古いためです（[CRIU](#criu) を参照）。CRIU がなくてもジョブは
実行できますが、チェックポイントには必要で、`gpueue doctor` は CRIU がない場合や古い場合に報告します。GPU
ジョブ用の cuda-checkpoint は [cuda-checkpoint](#cuda-checkpoint) で説明したように手動でインストールします。

`gpueue service install`（または `install.sh`）で gpueue をインストールしたマシンには、パッケージは
インストールを拒否します。まず `sudo gpueue uninstall` でそれを削除してください（このコマンドのない古い
gpueue では `install.sh --uninstall`）。実行中のジョブ、`/etc/gpueue`、`/var/lib/gpueue` は残ります。
`gpueue doctor` と `gpueue service install-method` はパッケージによるインストールをディストリビューションの
`package` として報告し、`gpueue service install`、`install.sh`、`gpueue update` はそれに手を出しません。

**更新。** どちらの方法でも、以降のバージョンはシステムの他の部分と一緒にリポジトリから入ります。

```sh
sudo apt update && sudo apt upgrade
```

リポジトリには amd64 と arm64 の安定版リリース（プレリリースは含みません）が、それぞれ最新の 5 バージョン
あります。アップグレードは動作中のサービスを再起動します。`gpueue update` と同様に、実行中のジョブは動き
続けます（[アップグレード](#upgrading) を参照）。`/etc/gpueue/config.toml` と
`/etc/apt/sources.list.d/gpueue.sources` の編集内容は保持されます。`/etc/apt/keyrings/gpueue.gpg` は、変更して
いなければ新しいバージョンの keyring で置き換えられます（新しい署名鍵はこうして届きます）。新しいリリースの
アップロード中は、`apt update` がハッシュの不一致やファイルの欠落で一時的に失敗することがあります。1 分ほど
して再実行すれば直ります。

**リポジトリの鍵。** パッケージ、リポジトリ、`.rpm` ファイルは 1 つの GPG 鍵（ソースツリーの
`packaging/apt/gpueue-archive-keyring.gpg`）で署名されています。フィンガープリントは
`9B4F 59D2 F414 B76B 626B 3BE0 4FB3 D15B 2389 5621` です。ダウンロードした鍵を信頼する前に確認するには
次のようにします。

```console
$ gpg --show-keys /etc/apt/keyrings/gpueue.gpg
pub   rsa4096 2026-10-15 [SC]
      9B4F59D2F414B76B626B3BE04FB3D15B23895621
uid                      gpueue APT repository <https://github.com/kaito4681/gpueue>
```

**削除。**

```sh
sudo apt remove gpueue    # stop and disable the services, remove the binaries and units
sudo apt purge gpueue     # also delete /etc/gpueue and the APT repository entry and key
```

`remove` はリポジトリのエントリーとその鍵（どちらも conffile）を残すので、`apt update` は引き続き動作し、
`sudo apt install gpueue` で gpueue を戻せます。どちらも `/var/lib/gpueue`（状態、ジョブのログ、チェック
ポイントのイメージ）、`gpueue` ユーザー、`gpueue-admin` グループを残し、実行中のジョブには決して触れません。
ジョブはそれぞれの `gpueue-job-*` ユニットで動き続けます。`purge` は `/var/lib/gpueue` の削除方法を表示します
（ジョブがまだ動いていればまず `sudo systemctl stop 'gpueue-job-*'`、その後 `sudo rm -rf /var/lib/gpueue`）。

<a id="the-apt-repository-maintainers"></a>
### APT リポジトリ（メンテナー向け）

リポジトリは、リリース `apt`（プレリリースなので、`install.sh` と `gpueue update` がたどる「最新」リリース
になることはありません）のアセットの集まりです。`.deb` ファイル、`Packages`、`Packages.gz`、`Release`、
`InRelease`、`Release.gpg`、公開鍵 `gpueue.gpg`、`gpueue.sources` です。リリースが**安定版リリースとして
公開されたとき**（`released` イベント：安定版リリースの公開、またはプレリリースの安定版への変更）、
`.github/workflows/release-apt.yml` が `scripts/dist/apt-repo.sh github <tag>` でリポジトリを更新します。
リリースの `.deb` ファイルを（チェックサムと、タグのコミットの公開鍵による minisign の署名を確認したあとで）
追加し、リポジトリにすでにあるパッケージを署名済みのインデックスと照合し（`InRelease` が
`packaging/apt/gpueue-archive-keyring.gpg` で検証でき、各 `.deb` が署名済みの `Packages` にある SHA256 と
一致すること。一致しなければジョブは何も変えずに失敗します。中断した実行が残した、インデックスにない `.deb` は
取り除きます）、アーキテクチャごとに最新の 5 バージョン（Debian のバージョン順）以外を削除し、インデックスを再生成
して署名し（保持する最新のパッケージから `gpueue.gpg` と `gpueue.sources` をコピーし）、パッケージを先に、
インデックスを最後に、`InRelease` を一番最後にアップロードします。古い `.deb` ファイルはその後で削除されます。
各アップロードは 3 回まで試します。それでもインデックスをアップロードできなければ、以前のインデックスファイルを
元に戻し、もう一度実行するよう伝えてジョブは失敗します。
`apt` リリースがなければ作成します。何を公開するかは `apt-repo.sh` が決めます。ドラフト、プレリリース、`v*`
以外のタグは通知を出してスキップされます。インデックスには `by-hash` ファイルがありません（サブディレクトリが
必要ですが、リリースアセットの名前には `/` を含められないため）。上で述べた短い時間帯があるのはこのためです。

`release.yml` はドラフトを作るだけなので、リポジトリが更新されるのは、人が GitHub でドラフトを公開したとき
（またはツールが personal access token で公開したとき）です。ワークフローの `GITHUB_TOKEN` で公開された
リリースは他のワークフローを開始しません。その場合は手動で実行してください。失敗したアップロードを再試行する
方法も同じです。

```sh
gh workflow run release-apt.yml -R kaito4681/gpueue -f tag=v0.1.0
```

本物のリリースなしで試すには、フォークを使います。そこで `scripts/dist/gen-apt-key.sh` で鍵を作り
（`release.yml` 用に minisign の鍵も）、公開鍵をコミットし、シークレットを設定し、タグを push し、ドラフトを
公開して、フォークの `apt` リリースに対して `apt update` を実行します。あるいはローカルのテスト
`scripts/apt-repo-test.sh`（`GPUEUE_APT_BASES="debian:12 ubuntu:22.04 ubuntu:24.04 ubuntu:26.04"`）を
実行します。これは `apt-repo.sh update DIR DEB...` と使い捨ての鍵でリポジトリを作り、コンテナ内でそれを
通してアップグレードします。

**アーカイブ鍵。** `packaging/apt/gpueue-archive-keyring.gpg` が本物の鍵で、フィンガープリントは
`9B4F 59D2 F414 B76B 626B 3BE0 4FB3 D15B 2389 5621` です（[Debian/Ubuntu パッケージのインストール](#installing-the-debianubuntu-package)
の「リポジトリの鍵」を参照）。その秘密鍵とパスフレーズはリポジトリのシークレット `APT_GPG_PRIVATE_KEY` と
`APT_GPG_PASSPHRASE` です。`release.yml` と `apt-repo.sh` は、ユーザー ID に "TEST KEY" とある keyring
（`scripts/dist/gen-apt-key.sh --test` が書き込むもの）を拒否します。鍵は次のように作ります。

```sh
scripts/dist/gen-apt-key.sh     # asks for a passphrase; writes the secret key under ~
gh secret set APT_GPG_PRIVATE_KEY -R kaito4681/gpueue < ~/gpueue-apt-key-*/apt-signing-key.asc
gh secret set APT_GPG_PASSPHRASE -R kaito4681/gpueue
# then commit packaging/apt/gpueue-archive-keyring.gpg
```

2 つのシークレットがなければ、`release-apt.yml` は（通知を出して）リポジトリをスキップします。インデックスは、
リポジトリ内の最新のパッケージが含む鍵でのみ署名されます（`apt-repo.sh` は `.deb` から
`/etc/apt/keyrings/gpueue.gpg` を読み取ります）。そのため、シークレットとコミットされた公開鍵が一致しない場合は、
全員の `apt update` を壊す代わりに実行が失敗します。

秘密鍵とそのパスフレーズはバックアップしてください。インストール済みのすべてのパッケージがこの鍵を信頼して
いるので、新しい鍵は、それを含むパッケージのバージョンを、古い鍵で署名されたリポジトリを通して配ることで
しか展開できません。

1. 新しい鍵を作り（作業用のチェックアウトで `scripts/dist/gen-apt-key.sh`）、その公開鍵を
   `packaging/apt/gpueue-archive-keyring.gpg` に追記して、keyring が両方の鍵を持つようにします
   （`cat old.gpg new.gpg >packaging/apt/gpueue-archive-keyring.gpg`）。シークレットは古い鍵のままにして、
   結合した keyring を含むバージョンをリリースします。
2. ユーザーがそのバージョンにアップグレードするまで待ちます（少なくとももう 1 回のリリースサイクル）。
3. シークレット `APT_GPG_PRIVATE_KEY` と `APT_GPG_PASSPHRASE` を新しい鍵に切り替え、同じリリースで keyring
   から古い鍵を削除します。keyring は conffile です。ユーザーが変更していなければ dpkg はアップグレード時に
   それを置き換えます（変更していれば dpkg が尋ねます）。手順 1 のバージョンを飛ばしたユーザーは
   `apt update` で `NO_PUBKEY` になるので、初回のインストールと同じように次の `.deb` を一度だけ手動で
   インストールします。

秘密鍵が漏洩した場合は、すぐに同じことを行い、ユーザーに新しい `.deb` を手動でインストールするよう伝えて
ください。

パッケージは `packaging/nfpm.yaml` から [nfpm](https://nfpm.goreleaser.com) でビルドされます
（`scripts/dist/build-package.sh BIN_DIR OUT_DIR [deb|rpm]` がビルド済みのバイナリから `.deb` か `.rpm` を
作り、`scripts/deb-test.sh` と `scripts/rpm-test.sh` がコンテナ内でそれらを確認します）。

<a id="installing-the-fedorarhelrocky-package"></a>
## Fedora/RHEL/Rocky パッケージのインストール

各リリースには、Fedora と RHEL 互換の 9 以降（RHEL、Rocky Linux、AlmaLinux。glibc 2.34 以降）向けに、
x86_64 と aarch64 の `.rpm` があります（どちらかは `uname -m` で分かります）。

**前提条件。**

- PID 1 としての systemd（パッケージが必要とします。systemd が PID 1 でなければサービスは有効化されるだけ
  です）。パッケージは dnf 自身がダウンロードします。`curl`（と `ca-certificates`）は検証用のファイルを
  ダウンロードするときだけ必要です。
- 任意：チェックポイント用の CRIU 4.2 以降（[CRIU](#criu) を参照）、GPU ジョブ用の NVIDIA ドライバーと
  cuda-checkpoint（[cuda-checkpoint](#cuda-checkpoint) を参照）。これらがなくてもジョブは実行できます。

最新リリースの `.rpm` を dnf でインストールします。dnf は URL を直接受け取れ、依存するものも一緒に入れます。

```sh
sudo dnf install "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue.$(uname -m).rpm"
```

`.deb` と同様に、`gpueue.<arch>.rpm` は最新の安定版リリースの `gpueue-<version>-1.<arch>.rpm` を
バージョンなしの名前にしたコピーで、特定のバージョン用にバージョン付きのファイルも添付されています
（`sudo dnf install https://github.com/kaito4681/gpueue/releases/download/v0.1.0/gpueue-0.1.0-1.x86_64.rpm`）。先に検証するには、
`gpueue.$(uname -m).rpm` を `.sha256` と `.minisig` ファイルと一緒にダウンロードして
[`.deb`](#installing-the-debianubuntu-package) と同じように確認し（trusted comment にはバージョンなしの
ファイル名が書かれています）、確認したファイルを `sudo dnf install "./gpueue.$(uname -m).rpm"` で
インストールします。

（`v0.2.0-rc.1` のようなプレリリースは `gpueue-0.2.0-rc.1-1.<arch>.rpm` です。パッケージ内のバージョンは
`0.2.0~rc.1` で、これは `0.2.0` より前に並びます。）

`.deb` と同じファイル（バイナリ、`/usr/lib/systemd/system` のユニット、sysusers.d と tmpfiles.d の
エントリー、`/etc/gpueue/config.toml`、シェルの補完、ドキュメント、`/usr/share/licenses/gpueue` のライセンス）
をインストールしますが、リポジトリの設定は含みません。設定は `%config(noreplace)` なので、アップグレード
しても編集内容は保持されます。初回のインストールでは `.deb` とまったく同じようにサービスを設定して起動し
（systemd が PID 1 でなければ有効化だけ行います）、`gpueue service install` や `install.sh` によるインストールと
並べてのインストールは拒否します。systemd と glibc 2.34 を必要とし、`criu` は**推奨**するだけです（弱い
依存関係で、ディストリビューションにあれば dnf がデフォルトでインストールします）。Fedora の CRIU（4.2）は
十分に新しいですが、RHEL/Rocky/Alma 9 の 3.19 は足りないので、そこでチェックポイントを取るにはより新しい
CRIU が必要です（[CRIU](#criu) を参照）。ジョブの実行は CRIU なしでもできます。SELinux が enforcing の
場合、ヘルパーはジョブのユニットが実行する自分自身のコピー（`/var/lib/gpueue/lib/gpueue-helper-<hash>`）に
`/usr/bin/gpueue-helper` 自身のラベルを付けます。パッケージは SELinux のファイルコンテキストの規則（policycoreutils-python-utils
の `semanage` が必要です）も、SELinux 関連の依存も追加しません。

**検証。** 他のリリースアセットと同様に、`.rpm` にも `.sha256` と `.minisig` ファイルがあります
（[リリースを手動で検証する](#verifying-a-release-by-hand) を参照）。GPG 鍵でビルドされたリリースでは
`.rpm` 自体にも署名され、その鍵（APT リポジトリの鍵）が `RPM-GPG-KEY-gpueue` として添付されます。この鍵ファイルも
他のアセットと同様に minisign で署名されているので、インポートする前にリリース鍵で確認してください。

```sh
curl -fsSLO https://github.com/kaito4681/gpueue/releases/download/v0.1.0/RPM-GPG-KEY-gpueue
curl -fsSLO https://github.com/kaito4681/gpueue/releases/download/v0.1.0/RPM-GPG-KEY-gpueue.minisig
minisign -Vm RPM-GPG-KEY-gpueue -P RWQhfodV2C486vD6glCl5MvvRupK9WboEzB2GUxcdmp2+NNqaL9W+UFP
gpg --show-keys RPM-GPG-KEY-gpueue                   # 9B4F59D2F414B76B626B3BE04FB3D15B23895621
sudo rpm --import RPM-GPG-KEY-gpueue
rpm -K gpueue-0.1.0-1.x86_64.rpm                     # "digests signatures OK"
# or let dnf refuse an unsigned or badly signed file:
sudo dnf install --setopt=localpkg_gpgcheck=1 ./gpueue-0.1.0-1.x86_64.rpm
```

**更新。** dnf リポジトリはまだありません（予定されています。[issue #23](https://github.com/kaito4681/gpueue/issues/23)
を参照）。そのため `dnf upgrade` だけでは新しいバージョンは見えません。代わりにリリースの新しい `.rpm` を
インストールしてください。dnf は URL を直接受け取れます。

```sh
sudo dnf upgrade "https://github.com/kaito4681/gpueue/releases/latest/download/gpueue.$(uname -m).rpm"
```

新しいリリースがあると、`gpueue doctor`（と `gpueue update --check`）の "update" の行に、マシンの
アーキテクチャに合ったリリースのバージョン付き URL を使った同様のコマンドと、先に署名を確認する方法が
表示されます。
アップグレードは動作中のサービスを再起動します。実行中のジョブは動き続けます。

**削除。**

```sh
sudo dnf remove gpueue
```

サービスを停止・無効化し、バイナリとユニットを削除します。`/var/lib/gpueue`（状態、ジョブのログ、チェック
ポイントのイメージ）、`gpueue` ユーザー、`gpueue-admin` グループは残し、実行中のジョブには決して触れません。
rpm には purge がありません。編集した `config.toml` は `/etc/gpueue/config.toml.rpmsave` として残ります。
不要になったら `/etc/gpueue` と `/var/lib/gpueue` を手動で削除してください（ジョブがまだ動いていれば、まず
`sudo systemctl stop 'gpueue-job-*'`）。

<a id="installing-with-nix"></a>
## Nix でのインストール

リポジトリは Nix flake（`flake.nix`）です。x86_64 と aarch64 の Linux 向けに gpueue をソースからビルドし、
GitHub リリースからは何もダウンロードしません。出力は次のとおりです。

| 出力 | 内容 |
| --- | --- |
| `packages.<system>.default`（`gpueue` も同じ） | `bin/gpueue`、`bin/gpueue-helper`、bash/zsh/fish の補完、ドキュメント |
| `packages.<system>.criu` | NixOS モジュール用の CRIU 4.2.1（nixpkgs のものは古いバージョン） |
| `devShells.<system>.default` | Rust ツールチェーン入りの開発用シェル（`nix develop`） |
| `nixosModules.default` | NixOS モジュール `services.gpueue`（[NixOS](#nixos) を参照） |
| `homeManagerModules.default` | home-manager モジュール `programs.gpueue`（[home-manager](#home-manager) を参照） |
| `packages.<system>.helper-tests` | `helper-tests` チェック用の gpueue-helper のテスト実行ファイル（ビルドのみで実行はしない） |
| `checks.<system>.*` | `home-manager`（モジュールの評価）と、x86_64 では KVM が必要な 2 つの NixOS VM テスト：`nixos`（モジュール）と `helper-tests`（gpueue-helper のすべてのテスト。ジョブを起動する helper のテストは Nix のビルドサンドボックス内では通らないため、パッケージビルド自体はテストを実行しない） |

リリースに追従するには `github:kaito4681/gpueue/stable` を使います。`stable` ブランチは、リリースが安定版
リリースとして公開されたとき、それが最も高いバージョンであれば、そのタグに移動します（ドラフト、プレリリース、
古い系列のパッチリリースには移動しません。`.github/workflows/release-nix.yml`）。単なる
`github:kaito4681/gpueue` は開発の先端（`main`）です。

<a id="nixos"></a>
### NixOS

flake の NixOS モジュール（`nixosModules.default`、ファイル `packaging/nix/nixos-module.nix`）は、デーモン、root
ヘルパー、CLI、`gpueue` システムユーザー、`gpueue-admin` グループ、`/etc/gpueue/config.toml`、
`/var/lib/gpueue` 以下のディレクトリを用意します。NixOS では `gpueue service install` や `gpueue update` を
実行しないでください。どちらも実行を拒否します（NixOS は `/etc/os-release` の `ID=nixos` で判別します。
[インストール方法](#install-method) を参照）。

システムの flake にこの flake を追加し（`stable` ブランチは最新のリリースに追従します）、サービスを有効に
します。

```nix
# flake.nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.gpueue.url = "github:kaito4681/gpueue/stable";
  inputs.gpueue.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, gpueue, ... }: {
    nixosConfigurations.gpu-server = nixpkgs.lib.nixosSystem {
      modules = [
        gpueue.nixosModules.default
        ./configuration.nix
      ];
    };
  };
}
```

```nix
# configuration.nix
{
  services.gpueue = {
    enable = true;
    admins = [ "alice" ];
    settings = {
      grace_secs = 60;
      checkpoint_size_limit_bytes = 500000000000;
    };
  };
}
```

その後 `sudo nixos-rebuild switch` を実行します。結果は `gpueue doctor` で確認できます。

| オプション | デフォルト | 意味 |
| --- | --- | --- |
| `services.gpueue.enable` | `false` | gpueue をインストールして起動します。 |
| `services.gpueue.package` | flake の `gpueue` | デーモン（`bin/gpueue`）とヘルパー（`bin/gpueue-helper`）を実行するパッケージ。`environment.systemPackages` にも追加されます。 |
| `services.gpueue.admins` | `[ ]` | `gpueue-admin` グループに追加するユーザー（[管理者](#administrators) を参照）。 |
| `services.gpueue.settings` | `{ }` | 属性セットで表した `/etc/gpueue/config.toml` の内容（キーについては [設定](#configuration) を参照）。 |
| `services.gpueue.criuPackage` | flake の `criu`（4.2.1） | ヘルパー用の CRIU。`criu_path` を設定します。nixpkgs 自体の `criu` は 4.2 より古いです。 |
| `services.gpueue.cudaCheckpointPackage` | `null` | `bin/cuda-checkpoint` を含むパッケージ。`cuda_checkpoint_path` を設定します。`null` の場合、GPU ジョブはチェックポイントできません。 |
| `services.gpueue.nvidiaSmiPath` | `hardware.nvidia.enabled` のときは `hardware.nvidia.package` の `nvidia-smi`、それ以外は `null` | `nvidia_smi_path` を設定します（ヘルパーはこれでジョブの CUDA プロセスを見つけます）。 |
| `services.gpueue.nvmlLibrary` | `hardware.nvidia.enabled` のときは `hardware.nvidia.package` の `libnvidia-ml.so.1`、それ以外は `null` | `nvml_library` を設定します。デーモンが GPU を検出するために読み込みます。 |

モジュールは `criu_path`、`cuda_checkpoint_path`、`nvidia_smi_path`、`nvml_library`、`shell` を
`lib.mkDefault` で設定するので、`settings` に書いた値が優先されます。NixOS での注意点：

- **ユニット。** `gpueue.service` と `gpueue-helper.service` は、モジュールの flake のリビジョンにある
  `packaging/systemd` のファイルで、`ExecStart=` はストアを指します。tmpfiles.d のエントリーは
  `packaging/tmpfiles.d/gpueue.conf` です。シャットダウン用ターゲットの `JobTimeoutSec=` を延ばすドロップイン
  （[シャットダウンのタイムアウト](#shutdown-timeouts) を参照）もそこから来ます。`package` を別のリビジョンにしても、
  ユニットはモジュールのリビジョンのものになります。2 つは揃えておいてください。
- **設定ファイル。** ヘルパーは root として動き、root 以外の誰も変更できない通常ファイルだけを読み、
  シンボリックリンクは決して読みません。そのためモジュールは、ファイルをストアにリンクする代わりに、NixOS に
  `/etc/gpueue/config.toml`（モード 0644、root の所有）へコピーさせます。
- **`nixos-rebuild switch` での再起動。** `settings`（またはパッケージ）を変更すると、デーモンとヘルパーの
  両方が再起動します。実行中のジョブは止まりません。ジョブは自分のユニットで動いているからです。再起動する
  ヘルパーはまず新しい起動を拒否し、処理中の操作を終わらせるので、チェックポイントやリストアの最中の switch は
  それを待ちます（ヘルパーの `TimeoutStopSec=`、30 分まで）。その間にデーモンが開始やリストアを試みたジョブは、
  失敗にはならずキューに戻され、新しいヘルパーが起動したら開始します。
- **更新と Nix のガベージコレクション。** ジョブのユニットは、そのジョブを開始したヘルパーのバイナリを実行し、
  チェックポイントのイメージにもそのバイナリが含まれます（リストアに必要です）。更新後、古いストアパスは
  もうシステムの一部ではないので、ヘルパーは、ジョブが動いている間、またはそのチェックポイントのイメージが
  残っている間、GC ルート `/nix/var/nix/gcroots/gpueue/job-<id>` でそれを保持します。ルートはジョブが終わるか、
  そのチェックポイントが削除されると取り除かれます。その後 `nix-collect-garbage` が古いバージョンを解放します。
  この機能より古いヘルパーが開始したジョブや取ったチェックポイントにはルートがありません。古いバージョンが
  回収されたあとにそれをリストアすると失敗するので、ガベージコレクションの前にそうしたジョブをリストアするか
  終わらせてください。
- **ジョブの環境。** ジョブはそれを投入した `gpueue add` の環境変数で動くので、`PATH` はユーザーのもの
  （`/run/wrappers/bin`、`/run/current-system/sw/bin`、`~/.nix-profile/bin` など）です。NixOS は
  `/usr/bin` に何も置かず、`/bin` には `sh` しかありません。モジュールは `shell = [ "/bin/sh" "-c" ]` を
  設定するので、ジョブの開始は `PATH` にまったく依存しません。
- **CRIU。** モジュールは flake の CRIU 4.2.1 をそのストアパスで使います。NixOS のカーネルは
  `CONFIG_CHECKPOINT_RESTORE=y` です。
- **NVIDIA。** NVIDIA ドライバーを NixOS の通常の方法で有効にしている場合
  （`services.xserver.videoDrivers = [ "nvidia" ]`、ヘッドレスのマシンでも同じ、または
  `hardware.nvidia.datacenter.enable`）、`nvidia-smi` と NVML は `hardware.nvidia.package` から取られます。
  NixOS では動的ローダーがドライバーのディレクトリを探さないためです。そうでない場合は `nvidiaSmiPath` と
  `nvmlLibrary`（または `settings.gpus`。[GPU の検出](#gpu-detection) を参照）を設定してください。
  ドライバーを更新すると、新しいストアパスによってデーモンが再起動します。
- **root が所有するパス。** `/nix/store` にあるプログラムは、ヘルパーと `gpueue doctor` の所有者チェックを
  通ります（[cuda-checkpoint](#cuda-checkpoint) を参照）。GPU のない VM では、`gpueue doctor` は失敗を報告
  しません。インストール方法は "Nix" で、警告が出るのは NVIDIA ドライバーと cuda-checkpoint のチェック
  だけです（後者のヒントは `services.gpueue.cudaCheckpointPackage` を挙げます）。

<a id="cuda-checkpoint-on-nixos"></a>
#### NixOS での cuda-checkpoint

cuda-checkpoint は nixpkgs になく、gpueue もパッケージ化していません（NVIDIA のバイナリです）。
derivation は次のように書けます。メジャーバージョンがドライバーと一致するリリースを使い、最初のビルドが
報告するハッシュを埋めてください。

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
services.gpueue.cudaCheckpointPackage = pkgs.callPackage ./cuda-checkpoint.nix { };
```

（`lib.licenses.unfree` には `nixpkgs.config.allowUnfree = true` か `allowUnfreePredicate` が必要です。）

<a id="updating-and-removing-on-nixos"></a>
#### NixOS での更新と削除

- 更新：`nix flake update gpueue && sudo nixos-rebuild switch`。変更されたユニットがデーモンとヘルパーを
  再起動します（上の「`nixos-rebuild switch` での再起動」を参照）。実行中のジョブは古いヘルパーのバイナリの
  まま動き続け、更新前に取ったチェックポイントは、`nix-collect-garbage` のあとでもリストアできます
  （「更新と Nix のガベージコレクション」を参照）。
- ロールバック：`sudo nixos-rebuild switch --rollback`（または古い世代での起動）は古いバイナリを戻しますが、
  古いデータベースは戻しません。データベースを移行した更新のあとでは、古いデーモンは新しいデータベースでの
  起動を拒否します。
- 削除：`services.gpueue.enable = false` を設定して `sudo nixos-rebuild switch` を実行します。ユニット、
  `/etc/gpueue/config.toml`、CLI はなくなりますが、`/var/lib/gpueue`（状態、ログ、チェックポイントの
  イメージ）と `gpueue` ユーザーのファイルは残ります。先に実行中のジョブを kill するか終わらせてください。
  ヘルパーは停止しますが、まだ動いているジョブのユニットはそのまま残されます。

<a id="home-manager"></a>
### home-manager

home-manager は 1 人のユーザーの環境を管理するものなので、デーモンやヘルパー（root とシステムの systemd が
必要）はインストールできません。そのモジュール（`homeManagerModules.default`）は、管理者が gpueue を
インストールしたマシン向けに CLI のシェル補完を用意します（`homeManagerModules.default`、ファイル
`packaging/nix/home-manager-module.nix`）。

```nix
{
  imports = [ inputs.gpueue.homeManagerModules.default ];
  programs.gpueue = {
    enable = true;
    # package = null;  # the default: use the system's gpueue
  };
}
```

| オプション | デフォルト | 意味 |
| --- | --- | --- |
| `programs.gpueue.enable` | `false` | モジュールを有効にします。 |
| `programs.gpueue.package` | `null` | `null`：何もインストールせず、シェルの起動時にシステムの `gpueue`（`PATH` にあるもの）の補完を読み込みます。その隣にインストールされた補完ファイル（NixOS やパッケージと同じく `<prefix>/share/...`）、それがなければ `gpueue completions <shell>` の出力です。パッケージ（例：`inputs.gpueue.packages.${pkgs.stdenv.hostPlatform.system}.default`）を指定すると、補完ファイルとともにプロファイルにインストールされます。 |
| `programs.gpueue.enableBashIntegration` | `true` | `package = null` のとき、Bash（`programs.bash.enable`）で補完を読み込みます。 |
| `programs.gpueue.enableZshIntegration` | `true` | Zsh についての同じ設定です（`programs.zsh.enable` と、`compinit` を実行する `programs.zsh.enableCompletion`。home-manager 25.05 以降）。 |
| `programs.gpueue.enableFishIntegration` | `true` | Fish についての同じ設定です（`programs.fish.enable`）。 |

CLI とデーモンは同じバージョンでなければなりません。`package = null` を推奨します。システムの `gpueue` は常に
デーモンと一致するからです。home-manager（または `nix profile`）の `gpueue` は、home-manager の flake が
ロックしているリビジョンのものなので、異なることがあります。その場合コマンドはバージョンの不一致で失敗し、
ストアパスを示したうえで、NixOS や home-manager のプロファイルでは `programs.gpueue.package = null` を
勧めます。更新は `nix flake update && home-manager switch` で行います。デーモンを更新するのは管理者です。

<a id="other-distributions"></a>
### その他のディストリビューション

flake をビルドし、その結果から tarball と同じようにサービスをインストールします。

```sh
nix build github:kaito4681/gpueue/stable
sudo ./result/bin/gpueue service install
```

`service install` はバイナリを `/usr/local/bin` にコピーし、それらはまだ Nix ストアからライブラリを読み込む
ので、ビルドをガベージコレクターのルートとして登録します（`/nix/var/nix/gcroots/gpueue`。
[インストール](#installing) を参照）。root の `PATH` に `nix-store` がない場合（sudo の `secure_path` は Nix の
プロファイルを含まないことがよくあります）、install は警告を出し、自分で登録するためのコマンドを表示します。
ルートが登録されていれば `./result` は削除してかまいません。ユニットは `/usr/local/bin` のコピーを実行するので、
インストール方法は `nix`（NixOS モジュールのように、`/nix/store` を指すユニットを意味します）ではなく
`service-install` です。CRIU は [CRIU](#criu) の説明に従ってインストールしてください。

更新するには、同じ 2 つのコマンドをもう一度実行します（Nix は現在の `stable` を取得します。1 時間以内に取得
していた場合は `--refresh` を付けてください）。実行中のジョブは動き続けます。`sudo gpueue update` も使えますが、
Nix のビルドをリリースの tarball のバイナリで置き換えます。gpueue の削除は `sudo gpueue uninstall` で行います
（[アンインストール](#uninstalling) を参照）。これは GC ルートも削除します。

`nix profile install github:kaito4681/gpueue/stable` でユーザーに入るのは `gpueue` コマンド（補完付き）だけ
です（[home-manager](#home-manager) も参照）。ヘルパーは root として動くのでユーザーのプロファイルからは実行
できず、サービスは上記のようにインストールする必要があります。

<a id="arch-linux-aur"></a>
## Arch Linux (AUR)

**AUR パッケージはまだ公開されていません。** 公開されるまでは、Arch Linux には `install.sh` で gpueue を
インストールしてください（Arch でも動作します。glibc 2.34 以降、systemd、x86_64 または aarch64 で、
minisign の署名も検証します）。CRIU 4.2 以降は公式リポジトリから入れます。

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/gpueue/releases/latest/download/install.sh | sudo bash
```

`sudo gpueue update` で更新し、`sudo gpueue uninstall` で削除します（[アップグレード](#upgrading) と
[アンインストール](#uninstalling) を参照）。この節の残りは、AUR に公開されたあとの AUR パッケージについての
説明です（メンテナー向けの設定は後述）。

AUR パッケージ [`gpueue-bin`](https://aur.archlinux.org/packages/gpueue-bin)（x86_64 と aarch64）は、
リリースの tarball をパッケージ化し直したものです。公開されたら、任意の AUR ヘルパー、たとえば yay で
インストールします。

```sh
yay -S gpueue-bin
```

または手動で：

```sh
git clone https://aur.archlinux.org/gpueue-bin.git
cd gpueue-bin
makepkg -si
```

`/usr/bin/gpueue` と `/usr/bin/gpueue-helper`、`/usr/lib/systemd/system` のユニットとシャットダウン用ターゲットの
ドロップイン（[シャットダウンのタイムアウト](#shutdown-timeouts) を参照）、`/usr/lib` の
`sysusers.d` と `tmpfiles.d` の断片、デフォルトの `/etc/gpueue/config.toml`（変更した場合はアップグレードや
削除のときも保持されます）、bash、zsh、fish の補完をインストールします。pacman の systemd フックが
`gpueue` ユーザー、`gpueue-admin` グループ、ディレクトリを作成します。Arch の慣例どおり、サービスは自動では
有効化も起動もされません。

```sh
sudo systemctl enable --now gpueue-helper.service gpueue.service
gpueue doctor
```

チェックポイントには CRIU 4.2 以降が必要で、これは公式リポジトリにあります（`sudo pacman -S criu`）。GPU
ジョブには NVIDIA の cuda-checkpoint も必要です（[cuda-checkpoint](#cuda-checkpoint) を参照）。どちらも
パッケージの任意の依存関係です。

- **更新：** `yay -Syu`（または `git pull && makepkg -si` でリビルド）。パッケージはサービスを再起動しない
  ので、その後 `sudo systemctl restart gpueue-helper.service gpueue.service` を実行して、デーモンと `gpueue`
  コマンドのバージョンを再び揃えてください。実行中のジョブは動き続けます（[アップグレード](#upgrading) を
  参照）。`gpueue update` と `gpueue service install` はパッケージによるインストールを拒否します。
- **削除：** `sudo systemctl disable --now gpueue.service gpueue-helper.service` のあと
  `sudo pacman -R gpueue-bin`。`gpueue-job-*` ユニットでまだ動いているジョブは、停止するまで動き続けます。
  `/var/lib/gpueue`（状態、ログ、チェックポイントのイメージ）と、変更した `/etc/gpueue/config.toml`
  （`config.toml.pacsave` として保存）は残ります。すべてを削除するには手動で削除してください。

<a id="the-aur-package-maintainers"></a>
### AUR パッケージ（メンテナー向け）

PKGBUILD は `packaging/aur/gpueue-bin` にあります。リリースが安定版リリースとして公開されると（APT
リポジトリと同じ `released` イベント。ドラフトやプレリリースでは動きません）、
`.github/workflows/release-aur.yml` が `scripts/dist/update-aur.sh <version>` を実行します。これは
リリースの tarball とその `.minisig` ファイルをダウンロードし、タグの `packaging/minisign.pub` で（`install.sh`
と同様に trusted comment `gpueue v<version> <file>` も）確認し、確認した tarball と、タグのソースアーカイブ
（署名付きのアセットには含まれないユニットと断片用）から PKGBUILD のチェックサムを計算し、Arch Linux の
コンテナで `.SRCINFO` を再生成します。そして `PKGBUILD`、`.SRCINFO`、
`gpueue-bin.install` を `ssh://aur@aur.archlinux.org/gpueue-bin.git` に push します。リリースには x86_64 と
aarch64 の両方の tarball が必要です。新しいリリースなしでパッケージングを変更するには、PKGBUILD の
`pkgrel` を上げてワークフローを手動で実行します（`gh workflow run release-aur.yml -f version=<X.Y.Z>`）。

最初に一度だけ行う設定：

1. <https://aur.archlinux.org> でアカウントを作ります。
2. 専用の SSH 鍵を作り、その公開鍵をアカウントに追加します（"My Account"、"SSH Public Key"）。

   ```sh
   ssh-keygen -t ed25519 -N '' -C gpueue-aur -f aur_gpueue
   ```

3. 秘密鍵をリポジトリのシークレット `AUR_SSH_PRIVATE_KEY` として保存し、ローカルのコピーは削除します
   （または安全な場所に保管します）。

   ```sh
   gh secret set AUR_SSH_PRIVATE_KEY -R kaito4681/gpueue < aur_gpueue
   ```

最初の push で AUR にパッケージが作られます。シークレットがなければ、ワークフローは通知を表示するだけです。
パッケージをローカルでテストするには（systemd の動く Arch Linux のコンテナで、makepkg、pacman、サービス、
ジョブ、アップグレードと削除）：`scripts/dist/aur-test.sh`。

<a id="building"></a>
## ビルド

2 つのバイナリが必要です。`gpueue`（CLI とデーモン）と `gpueue-helper`（root ヘルパー）です。単なる
`cargo build` では `gpueue` しかビルドされないので、両方のパッケージを指定します。

```sh
cargo build --release -p gpueue -p gpueue-helper
# -> target/release/gpueue, target/release/gpueue-helper
```

ローカルに Rust のツールチェーンがなければ、プロジェクトの Docker イメージ（Debian bookworm 上の Rust 1.98.1。
バイナリには glibc 2.34 以降が必要）でビルドできます。

```sh
scripts/test-in-docker.sh build --release -p gpueue -p gpueue-helper
# -> target/docker/release/gpueue, target/docker/release/gpueue-helper
```

タグ付きのバージョンは、x86_64 と aarch64 の Linux 向けのビルド済みバイナリ（Debian bookworm でネイティブに
ビルド、glibc 2.34 以降）としても GitHub のリリースページで配布されます。
`gpueue-v<version>-<target>.tar.gz`（`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`）に
`gpueue`、`gpueue-helper`、ドキュメントが入っており、隣に `.sha256` と `.minisig` ファイルがあります。
`install.sh` がこれらをインストールします（上記を参照）。手動では次のようにします。

```sh
sha256sum -c gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf gpueue-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo gpueue-v0.1.0-x86_64-unknown-linux-gnu/gpueue service install
```

<a id="installing"></a>
## インストール

```sh
sudo target/release/gpueue service install
```

`gpueue-helper` は `gpueue` の隣に置く必要があります。ユニットは次の場所のバイナリを実行します。

1. 再インストールや更新のときは、現在の `service install` によるインストール先のディレクトリ
2. そうでなければ、`gpueue` バイナリのあるディレクトリが `/usr/bin` か `/usr/local/bin` ならそのディレクトリ
   （その場で使い、何もコピーしません。このとき両方のバイナリと、そこにあるシンボリックリンクがたどるものは
   すべて root の所有で、グループやその他から書き込めない必要があります）
3. それ以外は `/usr/local/bin`。`gpueue` と `gpueue-helper` をここにコピーします（その旨を表示します）。ここは
   管理者がインストールするソフトウェアの通常の置き場所です。パッケージマネージャーは手を出さず、systemd と
   全ユーザーのデフォルトの `PATH` に含まれています。

`--bin-dir DIR` で別の場所にインストールできます。`gpueue-helper` は root として動くので、そのディレクトリと
すべての親は root の所有で、グループやその他のユーザーが書き込めない必要があります（まだ存在しない
ディレクトリはモード 0755 で作成されます）。それ以外と、`.` や `..` を含むパスは install が拒否します。
`--no-start` はサービスを開始も
再起動もせずに有効化します。`--force` は、`gpueue service install` が書いたのではないユニット、sysusers.d、
tmpfiles.d のファイルを上書きします（手順 2 を参照）。

RHEL、CentOS、Rocky、Alma、Fedora では、sudo の `secure_path` に `/usr/local/bin` が含まれないので、
`sudo gpueue ...` は "command not found" になります。`sudo /usr/local/bin/gpueue ...` を実行するか、
`sudo visudo` で `secure_path` に `/usr/local/bin` を追加してください。`gpueue doctor`（root で実行）がこれを
指摘します。最小限の `PATH`（cron の `/usr/bin:/bin` など）で動くスクリプトでもフルパスが必要です。

NixOS 以外で Nix でビルドした場合（`nix build` のあと `sudo ./result/bin/gpueue service install`）も、
バイナリは他のビルドと同じく `/usr/local/bin` にコピーされます。それらはまだ Nix ストアからライブラリを
読み込むので、install はストアパスをガベージコレクターのルート `/nix/var/nix/gcroots/gpueue` としても登録します
（`nix-store --add-root /nix/var/nix/gcroots/gpueue --realise <store path>` で）。`nix-store` が見つからないか
失敗した場合、install は警告を出してそのコマンドを表示し、以前に記録したルートは残します。実行するのは root 以外
が置き換えられない `nix-store` だけです（root の `PATH` にあるもの、なければ
`/nix/var/nix/profiles/default/bin/nix-store` か `/run/current-system/sw/bin/nix-store`）。ヘルパーはその
ローダーとライブラリで root として動くので、install は他人が変更できるストアにローダーがあるバイナリを拒否
します。ローダーとその上のすべてのディレクトリが root の所有で、グループやその他から書き込めないことが必要です。
ただし `/nix/store` 自体は、スティッキーでグループが `nixbld` なら（マルチユーザーの Nix インストール）グループ
書き込み可でも構いません（シングルユーザーのインストールのストアはそのユーザーのものです）。ルートがまだ必要か
どうかはインストールされたバイナリそのもの（動的ローダーが `/nix/store` にあるか）から判断されるので、Nix で
ビルドしたコピーを `--bin-dir` で移動してもルートは保持されます。`gpueue uninstall` はルートを削除し、ストアを
使わないバイナリ（たとえば `gpueue update`）をインストールしたときも削除されます。

`gpueue service install` は、最後の `gpueue doctor` のレポートに FAIL があると 0 以外で終了します。
`--no-start` のあとは、「まだ動いていない」ことを意味するだけの失敗（両方のユニット、両方のソケット、
デーモンとヘルパーの接続）は無視されます。

`gpueue service install` は何度実行しても安全です（たとえばアップグレードのあと）。次のことを行います。

1. `gpueue` と `gpueue-helper` を上で選んだディレクトリにコピーし（同じ内容の root 所有の通常ファイルが既に
   あればそのまま使い、それ以外、たとえばビルドディレクトリへのシンボリックリンクは置き換えます。モード 0755、
   アトミックに置き換え）、
   それらがストアから来たものなら Nix の GC ルートを登録し、`/var/lib/gpueue/install-receipt`（root、0644）を
   書き込みます。レシートにはディレクトリ、コピーしたバイナリ（その場で使う場合はなし）、バージョン、GC
   ルートが記録され、`gpueue uninstall` はまさにそれらを削除します。（`--bin-dir` で）別のディレクトリに
   再インストールすると、前回作ったコピーは削除されます。同じディレクトリの別名（`/usr` が統合された
   システムでの `/usr/bin` に対する `/bin` など）は同じディレクトリとみなされ、新しいバイナリと同じファイルで
   あるコピーは削除されません。レシートが存在するのに使えない場合（root の所有で
   ない、他のユーザーが書き込める、内容が不正）、install は何も変更せずに止まります。ファイルを直すか、
   記載内容を確認してから削除してください。
2. バイナリに組み込まれたパッケージングのテンプレートから次のファイルを書き込みます。各ファイルは
   "Installed by `gpueue service install`" というマーカー行で始まり、次回のインストールで上書きされます。
   マーカーのないファイル（管理者やパッケージが書いたもの）が既にあると、`--force` を指定しない限り、install は
   何かを変更する前に止まります。作成するディレクトリ（`/etc/gpueue` など）は umask にかかわらずモード 0755 に
   なります。
   - `/etc/systemd/system/gpueue-helper.service`：root ヘルパー（
     [packaging/systemd/gpueue-helper.service](../../packaging/systemd/gpueue-helper.service) から）
   - `/etc/systemd/system/gpueue.service`：デーモン。ユーザー `gpueue` として、NVML が必要とする NVIDIA の
     デバイスだけを許可する厳格なサンドボックスで動きます（
     [packaging/systemd/gpueue.service](../../packaging/systemd/gpueue.service) から）
   - `/etc/sysusers.d/gpueue.conf`：システムユーザー `gpueue` とグループ `gpueue-admin` を定義します
   - `/etc/tmpfiles.d/gpueue.conf`：下に挙げるディレクトリ
   - `reboot.target`、`poweroff.target`、`halt.target`、`kexec.target`、`soft-reboot.target` の
     `/etc/systemd/system/<target>.d/gpueue.conf`：シャットダウンにかけられる時間を延ばします（
     [packaging/systemd/shutdown-target-timeout.conf](../../packaging/systemd/shutdown-target-timeout.conf) から。
     [シャットダウンのタイムアウト](#shutdown-timeouts) を参照）
3. そのファイルについて `systemd-sysusers` を実行し、必要なら `groupadd --system gpueue-admin` を実行します。
4. そのファイルについて `systemd-tmpfiles --create` を実行し、次のものを作成します。

   | パス | モード、所有者 | 内容 |
   | --- | --- | --- |
   | `/var/lib/gpueue` | 0755 root | ベースディレクトリ（`data_dir`） |
   | `/var/lib/gpueue/daemon` | 0700 gpueue | デーモンのデータベース `state.sqlite` |
   | `/var/lib/gpueue/helper` | 0711 root | ヘルパーのジョブの記録とイベントキュー |
   | `/var/lib/gpueue/checkpoints` | 0700 root | チェックポイントのイメージ、ジョブごとに `<id>/` |
   | `/var/lib/gpueue/logs` | 0755 root | ジョブの出力、`<uid>/<id>.log`（ヘルパーが `<uid>/` をモード 2770、ユーザーの所有、グループ `gpueue` で作成） |
   | `/var/lib/gpueue/lib` | 0755 root | fork ガードのライブラリ（`gpueue-forkguard-<hash>.so`）と、ジョブのユニットが実行するヘルパーのコピー（`gpueue-helper-<hash>`。[アップグレード](#upgrading)を参照） |
   | `/var/lib/gpueue/criu-no-plugins` | 0755 root | manual モード用の空の CRIU プラグインディレクトリ |
   | `/run/gpueue` | 0755 gpueue | `gpueue.sock`（デーモン、モード 0666）と `helper.sock`（ヘルパー、`gpueue` の所有、モード 0600） |
   | `/run/gpueue-helper` | 0700 root | `criu restore --root` が使う `/` のバインドマウント |

5. `/etc/gpueue/config.toml` を [packaging/config.toml](../../packaging/config.toml) から作成します。ただし
   **まだ存在しない場合だけ**です。`nvidia-smi` が動けば、表示される各 GPU について `[[gpus]]` のエントリーを
   追記します（NVML が失敗したときのフォールバック。[GPU の検出](#gpu-detection) を参照）。
6. `systemctl daemon-reload` を実行します。ユニットかバイナリが変わっていれば、両方のサービスに
   `systemctl try-restart` を実行します。その後 `systemctl enable --now gpueue-helper.service gpueue.service`
   を実行し、デーモンがヘルパーに接続するまで最大 10 秒待って、`gpueue doctor` のレポートを表示します。

ディストリビューションのパッケージが所有するユニットファイル（`/usr/lib/systemd/...`）には決して触れません。
ユニットのカスタマイズはドロップイン（`sudo systemctl edit gpueue-helper.service`）で行ってください。
`/etc/systemd/system` のファイルを編集しても、次のインストールで失われます。

<a id="install-method"></a>
### インストール方法

`gpueue service install` は、パッケージマネージャーや Nix とマシンを共有してはいけません。`/etc/systemd/system`
のユニットがパッケージのユニットを覆い隠し、両方が `/etc/gpueue` と `/var/lib/gpueue` を使うことになるからです。
gpueue は、`gpueue.service` と `gpueue-helper.service` それぞれのユニットファイルで方法を見分けます。

| インストール方法 | 判別方法 |
| --- | --- |
| `gpueue service install` | インストールのマーカー行で始まる `/etc/systemd/system/<unit>` |
| ディストリビューションのパッケージ | `dpkg-query -S`、`rpm -qf`、`pacman -Qo` が所有者を示す `/usr/lib/systemd/system/<unit>`（または `/lib/systemd/system/...`）（`/usr` が統合されたシステムでは dpkg が `/lib` のほうを登録していることがあるので、両方のパスが存在すれば両方で問い合わせます。ディストリビューション自身のパッケージマネージャーから先に問い合わせます）。所有者を示したツールがあればそれで決まり、インストールされているすべてのツールがどのパッケージも所有していないと答えたときだけ、所有者のないユニットとみなされます。どのツールも答えられない場合（インストールされていない、またはロックされたデータベースなどでどれかが失敗した場合）、そのユニットは出所不明のパッケージとみなされます |
| 所有者のないユニット | パッケージマネージャーが所有されていないと確定的に報告する（`no path found matching pattern`、`is not owned by any package`、`No package owns`）そうしたユニット。削除したパッケージの残りか、手動で（`make install`）そこにコピーされたもの |
| Nix | `/nix/store` を指す `/etc/systemd/system/<unit>`（NixOS の `services.gpueue` モジュール、system-manager）、または `/nix/store` を指すか `/nix/store` のバイナリを実行するパッケージのユニット |
| 手動 | マーカーのない `/etc/systemd/system/<unit>` で、パッケージのユニットがないもの（`ExecStart=` が `/nix/store` 以下の場合も含む） |

パッケージの（または所有者のない）ユニットの隣にあるマーカーのない `/etc/systemd/system/gpueue.service` は、
管理者がそれを置き換えたもの（`systemctl edit --full gpueue`）で、そこにある `/dev/null` へのシンボリック
リンクはマスク（`systemctl mask gpueue`）です。どちらも 2 つ目のインストール方法ではありません。gpueue は
パッケージを "overridden" や "masked" として報告します。パッケージのないマスクは無視されます。

オペレーティングシステムは `/etc/os-release`（なければ `/usr/lib/os-release`）の `ID` と `ID_LIKE` から読み
取ります。これはヒントの文言と、NixOS（`ID=nixos`）ではモジュールを使うことを決めるだけで、インストール方法を
決めることはありません（同じディストリビューションでも、gpueue がパッケージから入っていることも `install.sh`
から入っていることもあります）。`/etc/NIXOS` は見ません。

各コマンドの動作（隠しコマンド `gpueue service install-method` が `install.sh` のためにキーワードを出力します）：

| インストール方法 | `service install` | `uninstall` | `update` | `doctor` | キーワード |
| --- | --- | --- | --- | --- | --- |
| 未インストール | インストールする | その旨を表示 | 拒否：`install.sh` を使う | INFO | `none` |
| 未インストール、NixOS | 拒否：`services.gpueue` モジュールを使う | その旨を表示 | 拒否：モジュールを使う | INFO "not installed (NixOS: use the services.gpueue module)" | `none` |
| `gpueue service install` | 再インストールする | アンインストールする | 更新する | OK | `service-install` |
| ディストリビューションのパッケージ（dpkg、rpm、pacman、または不明） | `--force` でも拒否 | 拒否 | 拒否 | OK（マスク時は INFO） | `package` |
| 所有者のないユニット | `--force` のときだけ。代わりに先にファイルを削除する | 拒否 | 拒否 | WARN | `unowned` |
| Nix | `--force` でも拒否 | 拒否 | 拒否 | OK | `nix` |
| 手動 | `--force` のときだけ | 拒否 | 拒否 | INFO | `manual` |
| 混在 | 拒否：先に `service install` のコピーをアンインストールする | `service install` が書いたものだけを削除（`--purge` なし） | 拒否 | FAIL | `mixed` |

「混在」とは、1 つのユニットで、または `gpueue.service` と `gpueue-helper.service` にまたがって（たとえば
デーモンのユニットは `service install`、ヘルパーのユニットはパッケージ）、`service install` がパッケージや Nix
と同時に使われている状態です。`doctor` は失敗し、`service status` は unhealthy と報告し、キーワードは `mixed`
です。`/usr/lib` の所有者のないユニットの隣にある `service install` は `service install` のままで（`/etc` の
ユニットが優先されます）、`service install` を再実行するのに `--force` は不要です。`doctor` は警告し、残骸を
削除するよう伝えます。2 つのユニットがそれ以外の異なる方法でインストールされている場合（たとえば片方が
ない、異なるパッケージマネージャーの異なるパッケージから来ている）、`doctor` は警告し、`service status` も
そう表示します。`service install` は両方をインストールするので、これで直ります。`/etc/systemd/system` 自体が
`/nix/store` を指している場合も、そこには書き込めないので `service install` は拒否します。

拒否するときは、インストール方法に合った gpueue の更新・削除の方法を示します。

| インストール方法 | 更新 | 削除 |
| --- | --- | --- |
| dpkg | `sudo apt update && sudo apt upgrade` | `sudo apt remove <package>` |
| rpm | `sudo dnf upgrade` | `sudo dnf remove <package>` |
| pacman | `yay -Syu`（AUR）または `sudo pacman -Syu` | `sudo pacman -R <package>` |
| 出所不明のパッケージ | お使いのパッケージマネージャー（分かれば `ID`/`ID_LIKE` から名前を示します） | 同じ |
| NixOS 上の Nix | `nix flake update && sudo nixos-rebuild switch` | `services.gpueue.enable = false` のあと `sudo nixos-rebuild switch` |
| それ以外の Nix | ユニットを作成した Nix の設定 | 同じ |
| `gpueue service install` | `sudo gpueue update` | `sudo gpueue uninstall` |

デーモンは `ProtectSystem=strict` で動くので、書き込めるのはユニットの
`ReadWritePaths=/var/lib/gpueue/daemon /var/lib/gpueue/logs /run/gpueue` 以下だけです。`config.toml` で
`data_dir`、`state_db`、`logs_dir`、`api_socket` を別の場所に移す場合は、新しいディレクトリをドロップインで
追加してください。そうしないとデーモンは "Read-only file system" で失敗します。

```sh
sudo systemctl edit gpueue.service
# [Service]
# ReadWritePaths=/srv/gpueue/daemon /srv/gpueue/logs
sudo systemctl restart gpueue.service
```

設定したディレクトリがユニットの実効的な `ReadWritePaths=` の外にあると、`gpueue doctor` が警告します
（`daemon write paths`）。

<a id="shutdown-timeouts"></a>
### シャットダウンのタイムアウト

システムのシャットダウン時に、ヘルパーがジョブのチェックポイントに使える時間は、systemd の 2 つのタイムアウトで
制限されます。

- **ヘルパーの `TimeoutStopSec=30min`** は、すべてのジョブをダンプするための初期の持ち時間です。ダンプの間、
  ヘルパーは systemd に時間の延長を求め続けます（`EXTEND_TIMEOUT_USEC`）が、それは実行中のダンプの期限と
  そのロールバックの終わりまでなので（[チェックポイントのタイムアウト](#checkpoint-timeouts) を参照）、
  止まったダンプがシャットダウンを永遠に引き止めることはありません。ジョブは 1 つずつダンプされます。
- **シャットダウン用ターゲット**（`reboot.target`、`poweroff.target`、`halt.target`、`kexec.target`、
  `soft-reboot.target`）**の `JobTimeoutSec=`** は、シャットダウン全体の上限です。シャットダウンがキューに
  入った時点から数えられ、これを過ぎると systemd はマシンを即座に強制停止します（`JobTimeoutAction=reboot-force`
  など）。`criu dump` の途中でも同じで、そのジョブにはチェックポイントが残りません。`TimeoutStopSec=` も
  ヘルパーの延長も、シャットダウンをこれより長くすることはできません。systemd の値は 30 分です。gpueue の
  すべてのインストール方法（`.deb` と `.rpm`、AUR パッケージ、NixOS モジュール、`gpueue service install`）は
  ドロップイン `<target>.d/gpueue.conf`（
  [packaging/systemd/shutdown-target-timeout.conf](../../packaging/systemd/shutdown-target-timeout.conf) から）を
  追加し、これを **2 時間** に延ばします。

2 時間あれば、期限が許す速度（30 s/GiB）で合計約 200 GiB の RSS + VRAM、NVMe ディスクで測定した速度
（約 2.5 s/GiB）なら数 TiB をカバーします。有効な値は次のように確認できます。

```sh
systemctl show -p TimeoutStopUSec gpueue-helper.service
systemctl show -p JobTimeoutUSec reboot.target poweroff.target halt.target kexec.target
```

ジョブがもっと大きいかディスクがもっと遅い場合は両方を延ばし、`JobTimeoutSec=` は `TimeoutStopSec=` に
他のサービスの停止にかかる時間を足したものより大きく保ってください。`/etc` にあり、並び順で `gpueue.conf` より
後になる名前のドロップイン（`systemctl edit` が書く `override.conf` など）が優先されます。

```sh
sudo systemctl edit gpueue-helper.service
# [Service]
# TimeoutStopSec=2h
for target in reboot poweroff halt kexec soft-reboot; do
  sudo mkdir -p /etc/systemd/system/$target.target.d
  printf '[Unit]\nJobTimeoutSec=4h\n' | sudo tee /etc/systemd/system/$target.target.d/override.conf
done
sudo systemctl daemon-reload
```

`JobTimeoutSec=infinity` にすると上限はなくなりますが、別の理由で止まったシャットダウンが自力で終わることも
なくなります。

<a id="checkpoint-timeouts"></a>
### チェックポイントのタイムアウト

チェックポイント全体（すべての CUDA プロセスの CUDA のロックとチェックポイント、続いて `criu dump`）や
リストア全体（`criu restore`、続いて CUDA のリストアとアンロック）は、ジョブのサイズに応じた期限内に終わる
必要があります。

```
deadline = checkpoint_deadline_base_secs + checkpoint_deadline_secs_per_gib * GiB
         = 180 s + 30 s * GiB            (defaults)
```

GiB は、チェックポイントではジョブの RSS + VRAM を切り上げたもの、リストアではイメージのサイズです。14 GiB の
ジョブなら 180 + 30 * 14 = 600 s になります。各ステップにもそれぞれのタイムアウトがあり（ロック：ロックの
`--timeout` 30 s + 15 s、CUDA のチェックポイントやリストア：300 s、アンロック：60 s）、
`criu_dump_timeout_secs` / `criu_restore_timeout_secs` で `criu` 単体に上限を設けることもできます。ステップには
先に終わるほうが適用されます。

期限を過ぎると、実行中のツールはそのプロセスグループ全体と fork したすべてのプロセスとともに kill されます。
その後：

- **チェックポイント**：ジョブは再開されます。停止したタスクに SIGCONT を送り、ジョブの cgroup 内で凍結された
  cgroup を解凍し、チェックポイントされた（またはされた可能性のある）CUDA プロセスをリストアし、ロックされた
  ものをすべてアンロックします。そしてチェックポイントは "the checkpoint did not finish within its deadline of
  ... s for ... GiB of memory; the job was resumed" で失敗します。途中までのイメージは削除されます。`criu` が
  外部から kill された場合（`kill -9`、OOM killer など）も同じロールバックが実行されます。各 CUDA プロセスは
  個別にロールバックされます。その間に終了したものはスキップされ、停止中や凍結中のものは（報告したうえで）
  そのままにされ、他のプロセスを妨げません。plugin モードでは、CRIU の CUDA プラグインが `criu dump` の中で
  CUDA プロセスをロックしてチェックポイントし、CRIU が kill されるとそれを元に戻せないので、ヘルパーが各
  プロセスに `cuda-checkpoint --get-state` で問い合わせ、必要に応じてリストアとアンロックを行います。期限を
  過ぎたその瞬間に終わった `criu dump` は、終わったものとして扱われます。

  注意：`criu dump` の kill は常に元に戻せるわけではありません。プロセスの状態を収集する間、CRIU はそのプロセスの
  中でパラサイトコードを実行します。その瞬間に CRIU が kill されると、プロセスはそのコードの途中で解放され、
  クラッシュすることがあります。したがって "the job was resumed" は、ロールバックが成功した（ジョブが解凍され、
  CUDA の状態が再開された）ことを意味し、ジョブのすべてのプロセスが生き残ったことは意味しません。`criu` が
  kill された場合は、メッセージにそう書かれます。
- **リストア**：リストアされたプロセスは kill され、リストアは失敗します。イメージは残ります。

NVMe ディスク（RTX 5070 Ti、38 GB RAM）で、ページキャッシュがコールドとウォームの場合の測定値：

| ステップ | 時間 |
| --- | --- |
| CUDA のロック / アンロック | 0.01〜0.04 s |
| CUDA のチェックポイント（VRAM からホストメモリへ） | 0.6〜1.0 s/GiB（VRAM 1.3〜11.8 GiB。11.8 GiB で 8〜10 s） |
| CUDA のリストア | 0.5〜0.9 s/GiB（11.8 GiB で 6〜8 s） |
| `criu dump` | 0.3〜1.8 s/GiB（RSS 1〜14 GiB。14 GiB で 18〜25 s、メモリ逼迫時はより遅い） |
| `criu restore` | 0.3〜0.7 s/GiB（14 GiB でウォーム 8.6 s、コールド 9.2 s） |

VRAM 11.8 GiB、RSS 1.2 GiB（13 GiB、期限 600 s）の GPU ジョブは、manual モードでも plugin モードでも同様に、
チェックポイントに 19〜20 s、リストアに 14〜16 s かかりました（plugin モード：`criu dump` 20〜21 s、
`criu restore` 16 s、CUDA のステップを含む）。つまりジョブは全体で RSS + VRAM 1 GiB あたり約 1.5〜2.5 s を
必要とし、デフォルトの期限には 10 倍以上の余裕があります（このジョブでは約 30 倍。約 35 MB/s のディスクでも
足ります）。より遅いストレージ（ネットワークファイルシステム、HDD）では `checkpoint_deadline_secs_per_gib` を
上げてください。CUDA のチェックポイントとリストアの固定の 300 s のタイムアウトは、測定した速度なら、プロセス
あたり VRAM 約 30 GiB までで 10 倍の余裕があります（同じ速度の 80 GB の GPU でも、その約 4 分の 1 の時間で
終わります）。

<a id="verifying"></a>
## 確認

```sh
gpueue doctor           # every check, with a `fix:` line for each problem; exits non-zero on FAIL
gpueue service status   # both units, and whether the daemon is reachable and connected to the helper
```

`gpueue doctor` は次のものをこの順に確認します（括弧内はチェックの名前）。

- 設定ファイルとその所有者。ヘルパーと同じ基準で判定します：ファイル、`/etc/gpueue`、`/etc`、`/` が root の
  所有で、グループやその他から書き込めず、シンボリックリンクでないこと（`config`）。満たさないファイルは
  読まず、デフォルトの値で報告します。
- インストール方法（`install method`、[インストール方法](#install-method) を参照）
- RHEL 系で gpueue が `/usr/local/bin` にある場合：sudo の `secure_path` にそのディレクトリが含まれるか
  （`sudo PATH`）
- `criu`、バージョン 4.2 以降（`criu`）
- NVIDIA ドライバー（`nvidia driver`）
- `cuda-checkpoint`：manual モードへの対応、ドライバーと一致するバージョン（`cuda-checkpoint`）
- plugin モードの場合のみ、CRIU の CUDA プラグイン（`cuda plugin`、
  [CUDA プラグインモード](#cuda-plugin-mode) を参照）
- ドライバーがある場合、デーモンが（NVML で、または `[[gpus]]` から）検出した GPU と `nvidia-smi` の比較
  （`gpus`、[GPU の検出](#gpu-detection) を参照）
- カーネル（`kernel`）と systemd のバージョン（`systemd`）
- 両方のユニット（`gpueue-helper.service`、`gpueue.service`）
- ヘルパーのバイナリとその上のすべてのディレクトリの所有者。ドロップインを含むユニットの実効的な
  `ExecStart=` から取得します（`helper binary`）
- ヘルパーが使う設定：`config.toml` の値で、ユニットのフラグがそれを上書きしている場合は警告します
  （`helper settings`）
- ジョブのユニットが自分の GPU のデバイスノードだけを開けるか（`gpu isolation`、
  [GPU の分離](#gpu-isolation) を参照）
- ジョブがシステムの D-Bus や systemd の Varlink サービスに到達できるか。`job_system_dbus` か
  `job_systemd_varlink` が有効なら警告します（`job d-bus`、
  [ジョブにできること・できないこと](#what-a-job-can-and-cannot-do) を参照）
- ジョブから隠すデーモンのソケットのディレクトリ。`job_hidden_socket_dirs` の既定の項目が抜けているとき、
  ディレクトリでない項目があるとき、nix-daemon の `trusted-users` が root 以外を挙げているのに
  `/nix/var/nix/daemon-socket` を隠していないときに警告します（`job sockets`、
  [ジョブにできること・できないこと](#what-a-job-can-and-cannot-do) を参照）
- ジョブを実行できるユーザー：最小の uid、拒否するグループ、許可するユーザー（`job policy`、
  [ジョブを実行できるユーザー](#who-jobs-may-run-as) を参照）
- デーモンのディレクトリが `ReadWritePaths=` の中にあるか（`daemon write paths`）
- デーモンとヘルパーのソケット（`daemon socket`、`helper socket`）
- デーモンとヘルパーのバージョンとその接続（`daemon`、`helper`）
- ヘルパーの起動時には見つからなかったが今はインストールされているプログラム（ヘルパーが使うのは再起動の後）、
  および起動時に見つけたが今はなくなっている、または root 以外が置き換えられるようになったプログラム
  （`helper programs`、ある場合のみ）。見つからなかったプログラムは設定した名前で照合します（たとえば
  `cuda_checkpoint_path = "cuda-checkpoint-r570"`）。CRIU と、NVIDIA ドライバーがあるホストでの
  cuda-checkpoint は FAIL です。ヘルパーが拒否するプログラム（再起動するとヘルパーが止まります）も FAIL で、
  再起動ではなく所有者の修正を案内します。それ以外は警告です。
- ヘルパーが実行しているのにデーモンが把握していないジョブ（`orphaned jobs`、ある場合のみ）
- チェックポイントのディレクトリ（存在するか、tmpfs でないか、空き容量）とチェックポイントの合計サイズ
  （`checkpoint dir`、`checkpoint usage`）
- あなたが管理者かどうか（`admin`）
- `update_check = false` でなければ、新しいリリースがあるかどうか（`update`）。このチェックは最大でも INFO で、
  GitHub に接続できない場合も同様です。

`criu check` は実行しません。このカーネルで CRIU が何に対応しているかは、自分で `sudo criu check` を実行して
確認してください。

ジョブを試してみます。

```sh
gpueue add -- 'for i in $(seq 1000); do echo $i; sleep 1; done'
gpueue checkpoint 0 && gpueue status
gpueue restore -i 0 && gpueue follow 0
```

<a id="testing-checkpointrestore-on-a-gpu-host"></a>
### GPU ホストでのチェックポイント/リストアのテスト

`scripts/gpu-host-test.sh`（ソースツリー内）は、マシンにインストールされている gpueue に対して、実際の
CUDA ジョブのチェックポイントとリストアをテストします。通常ユーザーとして実行し、root は不要で、自分の
ジョブと `~/.cache` 以下の一時ディレクトリ以外には何も変更しません。必要なのは `jq`、`nvidia-smi`、
C コンパイラ（`cc`、または `CC=...`）と NVIDIA ドライバーの `libcuda.so.1` です。テストプログラムは
インライン PTX を含む C から作られ、PTX はドライバーがその GPU 向けにコンパイルするので、CUDA
ツールキットは不要です。

```sh
scripts/gpu-host-test.sh                 # every scenario on GPU 0
scripts/gpu-host-test.sh --only a,c      # only some scenarios
scripts/gpu-host-test.sh --gpu 1         # another GPU
scripts/gpu-host-test.sh --keep          # keep the jobs and files for debugging
```

事前チェックでは `gpueue doctor` に FAIL がないことと GPU に空きスロットがあることを確認し、gpueue の
バージョン、ドライバー、GPU モデルを表示します。各ジョブは毎秒 4096 個の整数に `i+1` を加え（GPU
メモリ上、CPU ジョブではプロセスのメモリ上）、`value[i] == step*(i+1)` を確認するカウンターなので、
状態を失ったりリセットしたりするリストアは失敗します。シナリオは次のとおりです。

| | シナリオ |
|---|---|
| a | GPU ジョブ、チェックポイント/リストアを 3 回 |
| b | 一時停止した GPU ジョブをチェックポイントし、リストア後に Paused で戻り、`gpueue start` で続行する |
| c | CPU ジョブ、チェックポイント/リストアを 2 回 |
| d | GPU ジョブが Checkpointed のまま別の GPU ジョブが実行・終了し、その後リストアする |
| e | `--no-checkpoint` の GPU ジョブが Success で終わる |

ダンプのたびに、ジョブがイメージサイズ付きの Checkpointed であること、GPU スロットが空いていること、
`nvidia-smi` にそのプロセスがないこと、カウンターが止まっていることを確認します。リストアのたびに、
カウンターがダンプしたステップから続き、各ステップがちょうど 1 回ずつ数えられていることを確認します。
どのジョブも停止ファイルで止め、Success で終わる必要があります。失敗すると、ジョブの `last_error`、
ログの最後の行、およびジャーナルが読める場合（`adm` か `systemd-journal` グループ）はそのジョブに関する
`gpueue`/`gpueue-helper` のジャーナルの行を表示します。ジョブには実行ごとに一意なラベルが付き、
スクリプトの終了時に kill して削除されます。以前からあったジョブには触れません。どれかのシナリオが
失敗するとスクリプトは 0 以外で終了します。

メンテナーは毎回のリリース前に、リリース候補をインストールした GPU ホストでこれを実行します。

<a id="configuration"></a>
## 設定

デーモンとヘルパーは `/etc/gpueue/config.toml` を読みます。キーはすべて任意です。未知のキーは無視されます。
ファイルがなければすべてデフォルトになります。インストールされるファイル
（[packaging/config.toml](../../packaging/config.toml)）にはすべてのキーがコメント付きで並んでいます。
コメントアウトされた `# key = value` の行はデフォルトを示し、`# Example:` の下でインデントされた行は、
デフォルトでは未設定のキー（`checkpoint_size_limit_bytes` など）と `[[gpus]]` の例です。変更したら、その
キーを使うものを再起動してください。ヘルパーを再起動しても実行中のジョブには影響しません。

```sh
sudo systemctl restart gpueue-helper.service gpueue.service
```

ヘルパーは root として動き、ファイルとすべての親ディレクトリ（`/etc/gpueue`、`/etc`、`/`）が root の所有で、
グループやその他のユーザーが書き込めず、シンボリックリンクでない場合でなければ**起動を拒否します**。書き
込み先やコードの読み込み元のディレクトリにも同じ規則が適用されます。`data_dir`、`helper_state_dir`、
`logs_dir`、`checkpoints_dir`、`<data_dir>/criu-no-plugins`（`criu -L` に渡され、空でなければなりません）、
`<data_dir>/lib`（fork ガードとヘルパーのコピー）とそれらのすべての親は、root の所有で、グループやその他のユーザーが書き
込めない必要があります（設定ファイルと異なり、root が所有するシンボリックリンクはたどられ、リンク先も確認
されます）。ヘルパーはそこに何かを作る前にそれらを確認します。tmpfiles.d が作る配置を保ってください。
`data_dir` 自体は root のものでなければならないので、デーモンはデータベースを `daemon/` サブディレクトリに
保存します。

グループと GPU スロットの上限はここでは設定しません。それらはデーモンのデータベースに保存され、
`gpueue group`、`gpueue parallel`、`gpueue gpu parallel` で変更します。

ファイル、ディレクトリ、ソケットのパスは絶対パスでなければなりません。プログラム（`criu_path`、
`cuda_checkpoint_path`、`nvidia_smi_path`、`nvml_library`、`shell` の最初の単語）は、`PATH` から探される
名前だけか、絶対パスのどちらかです。`bin/criu` のような相対パスは拒否されます。ヘルパーは `criu_path`、
`cuda_checkpoint_path`、`nvidia_smi_path` を起動時に一度だけサービスの `PATH` から探し、見つかったファイルを
絶対パスと同じように検査し（root が所有し、root だけが変更できるディレクトリだけを通って到達できること）、
以後はそのパスを使います。見つからなかったプログラムは、ヘルパーを再起動するまで使えないままです（ログに
そう記録されます）。`*_secs` のキーはすべて最大 1 年（31536000）です。

| キー | デフォルト | 使用者 | 意味 |
| --- | --- | --- | --- |
| `data_dir` | `"/var/lib/gpueue"` | 両方 | `daemon/state.sqlite`、`helper/`、`logs/`、`checkpoints/`、`lib/`、`criu-no-plugins/` のベース |
| `state_db` | `<data_dir>/daemon/state.sqlite` | デーモン | SQLite のデータベース |
| `helper_state_dir` | `<data_dir>/helper` | ヘルパー | ジョブの記録とイベントキュー（root の所有） |
| `logs_dir` | `<data_dir>/logs` | 両方 | ジョブの出力、`<uid>/<id>.log`（root の所有） |
| `checkpoints_dir` | `<data_dir>/checkpoints` | 両方 | チェックポイントのイメージ（root の所有、tmpfs 不可） |
| `api_socket` | `"/run/gpueue/gpueue.sock"` | デーモン | クライアントが接続するソケット（クライアント側では `gpueue --socket` / `GPUEUE_SOCKET`） |
| `helper_socket` | `"/run/gpueue/helper.sock"` | 両方 | ヘルパーのソケット |
| `daemon_user` | `"gpueue"` | ヘルパー | ヘルパーへの接続を許可される唯一のユーザー |
| `grace_secs` | `30` | 両方 | ジョブを停止するときの SIGTERM から SIGKILL までの秒数 |
| `shell` | `["sh", "-c"]` | デーモン | ジョブのコマンドラインを後ろに付け加えるコマンドの接頭辞 |
| `max_unfinished_jobs_per_user` | `10000` | デーモン | 1 ユーザーが持てる、終了していないジョブ（待機中、stash 中、実行中、一時停止中、チェックポイント済みなど）の数。超えると `add` と、ジョブを再び動かす `restart` は `too_many_requests` で拒否されます |
| `max_job_spec_bytes` | `262144`（256 KiB） | デーモン | 1 つのジョブのコマンド、作業ディレクトリ、ラベル、環境変数（`gpueue add` は呼び出し元の環境変数をすべて送ります）の合計バイト数の上限。超えるジョブは `bad_request` で拒否されます |
| `checkpoint_size_limit_bytes` | 未設定（制限なし） | デーモン | 全イメージの合計がこのサイズを超えたら警告します（イメージが自動で削除されることはありません） |
| `checkpoint_size_warning_interval_secs` | `3600` | デーモン | 上限を超えている間、各管理者に通知する間隔 |
| `checkpoint_tcp_close` | `true` | 両方 | チェックポイント時に TCP 接続を閉じます（`criu --tcp-close`） |
| `criu_path` | `"criu"` | ヘルパー | 名前（ヘルパーの起動時に `PATH` から探す）または root が所有する絶対パス |
| `cuda_checkpoint_path` | `"cuda-checkpoint"` | ヘルパー | 同上 |
| `nvidia_smi_path` | `"nvidia-smi"` | ヘルパー | 同上 |
| `cuda_checkpoint_mode` | `"manual"` | ヘルパー | `"manual"`：ヘルパーが criu の前後で cuda-checkpoint を呼び出します。`"plugin"`：CRIU の CUDA プラグインがそれを行います。 |
| `checkpoint_deadline_base_secs` | `180` | ヘルパー | チェックポイント全体やリストア全体の期限の固定部分（[チェックポイントのタイムアウト](#checkpoint-timeouts) を参照） |
| `checkpoint_deadline_secs_per_gib` | `30` | ヘルパー | その期限のうち、RSS + VRAM（チェックポイント）またはイメージ（リストア）1 GiB あたりの部分 |
| `criu_dump_timeout_secs` | 未設定 | ヘルパー | `criu dump` 単体の任意の上限。未設定なら期限だけが適用されます |
| `criu_restore_timeout_secs` | 未設定 | ヘルパー | `criu restore` 単体の任意の上限。未設定なら期限だけが適用されます |
| `fork_guard` | `true` | ヘルパー | ジョブに fork ガードをプリロードします（README の制限事項を参照） |
| `gpu_device_isolation` | `true` | ヘルパー | 各ジョブのユニットが開けるデバイスノードを、割り当てた GPU のものに限ります（`DevicePolicy=closed`）。`false` で無効になります（[GPU の分離](#gpu-isolation) を参照） |
| `job_system_dbus` | `false` | ヘルパー | `true` にするとジョブがシステムの D-Bus（systemd、polkit）に接続できます。デフォルトではジョブから `/run/dbus` を隠します（[ジョブにできること・できないこと](#what-a-job-can-and-cannot-do)を参照） |
| `job_systemd_varlink` | `false` | ヘルパー | `true` にするとジョブが `/run/systemd` の下にある systemd の Varlink サービスに接続できます。デフォルトでは journald のログのソケット、resolved の名前解決のソケットと `resolv.conf` のファイル、`userdb/` 以外のこのディレクトリをジョブから隠します（[ジョブにできること・できないこと](#what-a-job-can-and-cannot-do)を参照） |
| `job_min_uid` | `/etc/login.defs` の `UID_MIN`、なければ `1000` | ヘルパー | uid がこれより小さいユーザーのジョブを拒否します（[ジョブを実行できるユーザー](#who-jobs-may-run-as)を参照） |
| `job_denied_groups` | `["docker", "lxd", "incus-admin", "libvirt", "libvirtd", "disk", "sudo", "wheel", "admin"]` | ヘルパー | root 相当のグループ。ジョブの補助グループから外し、プライマリグループがこれらのいずれかであるジョブは拒否します |
| `job_hidden_socket_dirs` | `["/run/libvirt", "/run/cups"]` | ヘルパー | 空の root 所有の tmpfs でジョブから隠すディレクトリ（なければ先に作ります）。呼び出し元のグループをユーザーデータベースで確かめるデーモンのソケットのもので、グループを外しても防げません。2 階層以上の絶対パスで指定し、経路上の既存のディレクトリ（とシンボリックリンク）は root のもので、グループや他人が書き込めないものでなければなりません（ヘルパーの起動時に確認します）（[ジョブにできること・できないこと](#what-a-job-can-and-cannot-do)を参照） |
| `job_allowed_users` | `[]`（全員） | ヘルパー | 空でなければ、ここに（名前で）挙げたユーザーだけがジョブを実行できます |
| `gpu_detection` | `"auto"` | デーモン | `"auto"`：NVML、失敗したら `[[gpus]]`。`"static"`：`[[gpus]]` のみ。 |
| `nvml_library` | 未設定 | デーモン | NVML ライブラリのパスまたは名前。デフォルトでは `libnvidia-ml.so.1`、次に `libnvidia-ml.so` |
| `update_source` | `"github:kaito4681/gpueue"` | CLI | `gpueue update` と `gpueue doctor` がリリースを探す場所：`github:<owner>/<repo>` |
| `update_check` | `true` | CLI | `gpueue doctor` が `update_source` に新しいリリースを問い合わせるかどうか |
| `[[gpus]]` | 空 | デーモン | 静的な GPU のリスト：`index`（必須）、`uuid`、`name`、`memory_total_bytes`（[GPU の検出](#gpu-detection) を参照） |
| `[priority]` | フェアシェアを含むマルチファクター優先度 | デーモン | キューにあるジョブの順序の決め方（[優先度とフェアシェア](#priority-and-fair-share) を参照） |

`gpueue-helper serve` はコマンドラインのフラグも受け付けます（`gpueue-helper serve --help`）。フラグはファイルより
優先されます。ファイルを使うことを推奨します。`gpueue doctor` はファイルの値を報告し、ユニットの `ExecStart=`
がフラグを渡していると警告します。

<a id="administrators"></a>
## 管理者

管理者は root と `gpueue-admin` のメンバー（プライマリまたは補助グループ）です。全ユーザーのジョブを操作でき、
管理者専用のコマンド（`group add/remove`、`parallel`、`gpu parallel`、`pause/start/kill -g` や `-a` による
グループの一時停止/再開、`shutdown`、全ジョブの `reset`、優先度の設定 `share set`、`group priority`、
`site`、負の値の `nice`）を実行できます。

```sh
sudo usermod -aG gpueue-admin alice
```

デーモンはリクエストごとに（NSS を通して）グループを調べるので、変更はログインし直さなくてもすぐに反映
されます。

<a id="who-jobs-may-run-as"></a>
## ジョブを実行できるユーザー

root ヘルパーがすべてのジョブをその所有者として起動するので、誰のジョブを実行するかはヘルパーが決めます。
デーモンにはそれを変えられません。ジョブを root（uid 0 や gid 0）や所有者が属していないグループで実行しない
ことに加えて、ヘルパーはすべての起動とリストアに 3 つの設定を適用します。

- `job_min_uid`：uid がこれより小さいユーザーは拒否されます。未設定のときは `/etc/login.defs` の `UID_MIN`
  （ほとんどのディストリビューションで 1000）、ファイルに有効な `UID_MIN` がなければ 1000 です。これにより
  システムアカウント（`www-data`、`postgres`、デーモン自身の `gpueue` ユーザーなど）はジョブを実行できません。
  この値は、どのジョブの所有者も変更できないディレクトリをヘルパーが判断するのにも使われます。CRIU がリストア
  したジョブにバインドするホストのパスは、uid がこれより小さいアカウントのディレクトリ（`systemd-resolve` の
  `/run/systemd/resolve` など）の下にあってもかまいませんが、ジョブを実行できるユーザーのディレクトリや
  デーモンのユーザーのディレクトリの下にあってはいけません（[応答しなくなったネットワークと FUSE のファイルシステム](#network-and-fuse-filesystems-that-stop-answering)を参照）。
- `job_denied_groups`：メンバーがパスワードなしで root として振る舞えるグループです。これらはジョブの補助
  グループから外されるので、`docker` に属するユーザーのジョブは `docker` なしで実行されます。どのグループを
  外したかはヘルパーのジャーナルに記録されます（ジョブの出力には何も加えません）。プライマリグループがこれらの
  いずれかであるジョブは拒否され、プロセスがこれらのいずれかを持っていたチェックポイントのリストアも拒否
  されます（CRIU はそのグループを復元してしまうため）。名前は起動とリストアのたびに `/etc/group` で調べ、
  そこにない名前は無視します。gid 0 は常に外されます。既定値とそれぞれを含める理由は次のとおりです。

  | グループ | root 相当である理由 |
  | --- | --- |
  | `docker` | Docker のソケットから、`/` をバインドマウントして root で動くコンテナを起動できる |
  | `lxd`、`incus-admin` | LXD/Incus で同じことができる（特権コンテナ、ホストのマウント）。Incus の制限付きの `incus` グループは含めていない |
  | `libvirt`、`libvirtd`（NixOS） | `qemu:///system` を管理でき、その VM にホストのブロックデバイスやディレクトリを渡せる |
  | `disk` | ブロックデバイスに直接読み書きでき、つまりその上のすべてのファイルを読み書きできる |
  | `sudo`、`wheel`、`admin` | sudo の権限。sudo は通常パスワードを尋ね、無人のジョブはそれを入力できないが、`NOPASSWD` の規則やキャッシュされた認証があればジョブが root になれてしまう |

  root に相当しないので含めていないもの：`shadow`（パスワードハッシュ。せいぜいオフラインで解析される）、
  `adm`（ログ）、`kvm`、`video`/`render`（デバイス。GPU ジョブには後の 2 つが必要）。ホストで root 相当になる
  グループ（例：`microk8s`）を追加するか、ジョブに使わせたいグループを外してください（例：
  `job_denied_groups = ["lxd", "incus-admin", "libvirt", "libvirtd", "disk", "sudo", "wheel", "admin"]` でジョブが
  Docker を使えるようになります）。
- `job_allowed_users`：空でなければ、（名前で）挙げたユーザーだけがジョブを実行できます。

拒否されたジョブは、`gpueue refuses to run jobs as <user>:` で始まり、たとえば `uid 999 is below job_min_uid
1000 (UID_MIN in /etc/login.defs)`、`primary group docker is in job_denied_groups`、`not in
job_allowed_users` が続く理由で起動に失敗します。`gpueue status` には `Failed to spawn` と表示され、
`gpueue log <id>` と `gpueue status --json` で理由が分かります。拒否されたリストアはリストアの失敗として扱われ
（[リストアの失敗](#restore-failures)を参照）、ジョブはチェックポイント済みのままです。`gpueue doctor` は有効な
ポリシー（最小 uid とその出どころ、拒否するグループとそのうちホストに存在するもの、許可するユーザー）を表示し、
ヘルパーも起動時にログに記録します。これらのキーを変えたらヘルパーを再起動してください。

<a id="what-a-job-can-and-cannot-do"></a>
## ジョブにできること・できないこと

ジョブは所有者として実行され、ファイル、ネットワーク、割り当てた GPU に対して所有者ができることはできます。
gpueue は、所有者が gpueue の外ではできる場合（`NOPASSWD` ルールのある `sudo` のメンバー、`docker` のメンバー、
polkit が信頼するグループのメンバー）でも、ジョブがそれ以上の権限を得られないことを保証します。チェックポイント
から再開したジョブも同じです。リストアしたユニットも同じように設定され、リストアしたプロセスは資格情報を
そのまま保ちます。e2e のシナリオ 19 は、チェックポイントとリストアの前後で以下の各項目を確かめます。

| ジョブは… | 理由 | 切り替え |
| --- | --- | --- |
| 所有者の uid とプライマリ gid で実行され、root にはならない | ヘルパーが uid 0、システムアカウント、拒否するプライマリグループを拒否する（[ジョブを実行できるユーザー](#who-jobs-may-run-as)） | `job_min_uid`、`job_denied_groups`、`job_allowed_users` |
| ケーパビリティを持たず（`CapInh`、`CapPrm`、`CapEff`、`CapBnd`、`CapAmb` がすべて 0）、`NoNewPrivs: 1` | shim がコマンドを実行する前にバウンディングセットを空にし、`no_new_privs` を設定する | なし |
| `sudo`、`su`、`pkexec` などの setuid やファイルケーパビリティのプログラムで権限を得られない | `no_new_privs` のもとではカーネルが setuid ビットとファイルケーパビリティを無視する | なし |
| システムの D-Bus に接続できないので、`systemctl`、`run0`、`machinectl`、`busctl --system` など、systemd、polkit、システムサービスへの要求はできない | shim がジョブのマウント名前空間で `/run/dbus`（`/var/run` が `/run` へのリンクでなければ `/var/run/dbus` も）を空の root 所有の tmpfs で隠す。ホストにまだそのディレクトリがなければ作るので、後から現れるバスのソケットにも接続できない。リストアでは CRIU が tmpfs を保存して作り直す。polkit はグループの所属をジョブのグループではなくユーザーデータベースから判断するので、グループを外すだけでは防げない | `job_system_dbus = true` |
| systemd の Varlink サービス（`io.systemd.Manager` の reload と reexecute、`io.systemd.Credentials` のシステム資格情報の復号、mountfsd、nsresourced、machined、logind、networkd、sysupdate など）に接続できない。これらは D-Bus を通らずに polkit で一般ユーザーの呼び出しを認可する | shim が `/run/systemd` を空の root 所有の tmpfs で隠し、journald のログのソケット（`journal/socket`、`journal/stdout`、`journal/dev-log`）、resolved の `resolve/io.systemd.Resolve`、`resolve/stub-resolv.conf`、`resolve/resolv.conf`（1 つずつバインドするので、後からそこに作られたソケットには接続できない）、`userdb/` ディレクトリ（ユーザーとグループの検索だけ。後述）と空の `system/` だけを残す。リストアでは CRIU がこれを保存して作り直す | `job_systemd_varlink = true` |
| 呼び出し元のグループをユーザーデータベースで確かめて認可するデーモンのソケットに接続できない。libvirt のもの（`/run/libvirt/libvirt-sock`、`virtqemud-sock`、`virtstoraged-sock` など。`-ro` と `-admin` も）は Fedora、RHEL、Debian、Arch、NixOS では誰でも接続でき、polkit で認可される。同梱の polkit ルールは `org.libvirt.unix.manage` をグループ `libvirt`（NixOS では `libvirtd`）に許すので、隠さなければメンバーのジョブがホストのディスクを使う VM やストレージプールを定義できる。CUPS の `/run/cups/cups.sock` では、cupsd がピアの資格情報で呼び出し元を判断し、`SystemGroup`（`lpadmin`、`wheel`）のメンバーにプリンターの管理を許す | shim が `job_hidden_socket_dirs` の各ディレクトリを `/run/dbus` と同じく空の root 所有の tmpfs で隠す（なければ作るので、後から現れるソケットにも接続できない。リストアでは CRIU が保存して作り直す）。libcups は `localhost:631` に切り替えるので印刷はでき、そこでの管理にはパスワードが要る | `job_hidden_socket_dirs` |
| `docker`、`lxd`、`incus-admin`、`libvirt`、`libvirtd`、`disk`、`sudo`、`wheel`、`admin` のグループを持たない（root のデーモンのソケット、ブロックデバイスそのもの） | ヘルパーが補助グループから外す | `job_denied_groups` |
| 標準の疑似デバイスと割り当てた GPU のもの以外のデバイスノードを開けない | `DevicePolicy=closed` と `DeviceAllow=`（[GPU の分離](#gpu-isolation)） | `gpu_device_isolation` |

それぞれの切り替えは保証を 1 つ弱めます。`job_system_dbus = true` か `job_systemd_varlink = true` のとき、`job_hidden_socket_dirs` から既定の項目が抜けているときは `gpueue doctor` が警告します。
gpueue が制限しないもの：所有者の uid そのものにできること（ファイル、セッションバス経由のユーザーサービス、
ネットワーク）、ジョブが使う CPU、メモリ、ディスクの量、自分で資格情報を確かめるほかのサービスのソケット
（たとえば、どのユーザーにも許すデーモンのソケット）。ジョブのマウントはプライベート（ホストからの伝播なし）なので、
ホストが後から `/run/dbus`、`/run/systemd`、その `journal/` と `resolve/` に作ったソケットは、リストアの前でも後でも
ジョブには現れません（e2e のシナリオ 19 で確かめます）。ディレクトリごとバインドするのは `userdb/` だけです。そこにある
サービスは `io.systemd.UserDatabase`（`GetUserRecord`、`GetGroupRecord`、`GetMemberships`）だけを実装する読み取り専用の
検索で、`getent` でも分かることしか答えないので、後から加わったソケットも何も許しません。ほかの root のソケットには
接続できますが、一般ユーザーには何も許しません。udev の `/run/udev/io.systemd.Udev` は root しか受け付けず（ソケットの
モードは 0644）、polkit の `/run/polkit/agent-helper.socket` はクライアントが D-Bus 経由で始めた polkit の認証を完了させる
だけで（ジョブは D-Bus に接続できません）、システムバスはほかのアドレスでは待ち受けません
（`<listen>unix:path=/run/dbus/system_bus_socket</listen>` だけ）。名前解決は使えますが、ジョブには開始したときの
`resolv.conf` が見えます（ファイルをバインドしており、resolved は変更時にファイルを置き換えるため）。

ほかの場所にあるデーモンのソケットは Ubuntu 22.04/24.04/26.04、Debian 12、Fedora、RHEL 9、Arch、NixOS で
確かめました。ほとんどはソケットファイルのグループで判断するので、グループを外せば防げます。Docker
（`/run/docker.sock`、グループ `docker`）、Incus（`/var/lib/incus/unix.socket`、グループ `incus-admin`。
`unix.socket.user` はグループ `incus` で、制限付きのプロジェクトしか与えません）、LXD
（`/var/snap/lxd/common/lxd/unix.socket` と Ubuntu の `/run/lxd-installer.socket`、グループ `lxd`）、Ubuntu の libvirt
（`auth_unix_rw = "none"` でグループ `libvirt` の `libvirt-sock`。いずれにせよ隠します）がそうです。rootful の Podman の
`/run/podman/podman.sock`、containerd、virtlogd と virtlockd、Cockpit の内部のソケットは root かシステムグループしか
受け付けません。Cockpit そのもの（ポート 9090）と SSH は、ほかのログインと同じくユーザーのパスワードか鍵を求めます。
udisks2、NetworkManager、PackageKit、accounts-daemon、fwupd、firewalld には D-Bus のインターフェースしかありません。
snapd の `/run/snapd.socket` には誰でも接続できますが、snap のインストール、削除、設定には polkit の `auth_admin`
が要り、それにはシステムバス上の認証エージェントが要るので、ジョブにはできません（そもそも snap は `no_new_privs`
のもとでは動きません）。nix-daemon（`/nix/var/nix/daemon-socket/socket`）は `trusted-users` の `@group` を
グループデータベースで確かめ、信頼されたユーザーは root 相当です。既定（`root` だけ）は安全ですが、`@wheel`
やほかのユーザーを信頼するなら `/nix/var/nix/daemon-socket` を `job_hidden_socket_dirs` に加えてください
（`gpueue doctor` が警告します）。ジョブからは Nix デーモン（ビルド、取得）を使えなくなります。グループや polkit
のルールを信頼するサイト独自のソケットも、同じようにそのディレクトリを加えてください。ホストがディレクトリを
削除すると、その上のマスクは外れます（削除されたディレクトリ上のマウントは、カーネルがすべての名前空間で外します）。
ホストが残すディレクトリを挙げてください。

<a id="priority-and-fair-share"></a>
## 優先度とフェアシェア

キューにある複数のジョブが同じグループや GPU のスロットを待っているとき、gpueue は収まるジョブのうち
優先度が最も高いものを開始します（収まらないジョブが、その後ろにある小さなジョブを妨げることはありません）。
優先度は Slurm の
[multifactor priority プラグイン](https://slurm.schedmd.com/priority_multifactor.html) に従います。

```text
priority = site factor
         + priority_weight_age       * age factor
         + priority_weight_fairshare * fair-share factor
         + priority_weight_job_size  * job size factor
         + priority_weight_partition * partition factor
         + priority_weight_tres      * TRES factors
         - nice
```

各要素は 0 から 1 の値です。

- **待ち時間（age）：** ジョブが実行可能になってから（キューにあり、すべての依存関係が成功で終わってから）の
  時間を `priority_max_age_secs` で割ったもので、最大 1 です。stash されたジョブと依存関係を待っている
  ジョブは待ち時間が増えません。
- **フェアシェア：** Slurm の古典的な式 `F = 2^(-U/S)` です。`S`（正規化したシェア）は、そのユーザーの
  シェアを*アクティブな*全ユーザー、つまり未完了のジョブか記録された使用量があるユーザーのシェアの合計で
  割ったものです（管理者が別の数を設定しない限り、ユーザーのシェアは 1 です）。`U`（実効使用量）は、
  そのユーザーの使用量を全ユーザーの使用量で割ったものです（誰も何も使っていない間は 0）。ちょうど自分の
  シェア分だけ使ったユーザーは 0.5、シェアより少なければそれより大きく、多ければ小さくなります。
  シェアが 0 のユーザーは 0 です。
- **ジョブのサイズ：** ジョブの GPU 数をマシンの GPU 数で割ったものです。CPU のみのジョブでは 1 CPU を
  マシンの CPU 数で割ったものです（gpueue は CPU を割り当てないので、どのジョブも 1 CPU として数えます）。
  `priority_favor_small = true` では代わりに `(total - requested + 1) / total` となり、最も小さいジョブが
  1 になります。
- **パーティション：** gpueue のグループが Slurm のパーティションの役割を果たします。管理者は
  `gpueue group priority <group> <n>` でグループに優先度を付けます。要素はその優先度を最も高いグループの
  優先度で割ったものです（すべてのグループの優先度が 0 の間は 0）。
- **TRES：** `priority_weight_tres` で重みを付けた各リソース（`CPU`、`GRES/gpu`）について、ジョブの要求量を
  マシンの総量で割ったものです。

**使用量。** 課金されるのはジョブが実行している時間だけです。ジョブの課金量（`tres_billing_weights` による。
デフォルトでは CPU 1 つあたり 1、GPU 1 つあたり 10）に実行した秒数を掛けたものが所有者の使用量に加えられます。
ジョブは実行を止めたとき（一時停止、チェックポイント、終了、kill、消失したとき）と、実行中は
`priority_calc_period_secs` ごとに（およびデーモンの起動時に）課金されます。キューで待つ時間、一時停止中、
チェックポイント済み、復元待ちの時間は課金されません。また、1 回の課金は最大 2 計算期間分なので、デーモンが
動いていなかった時間は課金されません。各計算では、すべての使用量を `priority_decay_half_life_secs` の
半減期で減衰させもします（0：減衰なし）。スケジューラーは、各計算時とシェアが変わったときに計算した
フェアシェアの要素を使います。その間に最初のジョブを投入したユーザーは、次の計算まで使用量なしとして
扱われます。使用量は再起動をまたいでデータベースに
保存され、`priority_usage_reset_period` に従ってリセットされます（`"now"` はデーモンが起動するたびに
リセットし、それ以前の分は、実行中のままのジョブのそれまでの実行時間も含めて課金しません。暦の期間は
ローカル時刻の 00:00 にリセットし、週は日曜日に始まります。夏時間の開始日・終了日も期間ごとに 1 回だけ
リセットします）。0.001 課金秒未満の使用量は破棄されます。

**ユーザー自身の順序。** pueue の `-o/--priority`、`gpueue switch`、`gpueue top` は、HTCondor の
`condor_prio` や Slurm の `scontrol top` と同じく、1 人のユーザーのジョブだけを並べ替えます。順序は、
先頭に移したジョブ（最も新しい `top` が先）、次に `-o` の優先度が高いもの、次に id が小さいものです。
gpueue はこれを、ユーザーの待機中のジョブのうち今すぐ起動できるものの待ち時間をまとめ、この順に
割り当て直す（最も長い待ち時間を最初のジョブに）ことで適用します。そのため、そのユーザーの優先度の
合計も、他のユーザーの位置も変わりません。今すぐ起動できるジョブとは、グループが存在して一時停止
されておらず、要求したすべての GPU が検出されていて、依存先が完了しているジョブです。グループや GPU の
空きスロットを待っているだけのジョブは通常の順番待ちなので除外されません。今すぐ起動できないジョブは、
そのユーザーの他のジョブに待ち時間を貸しません。自分の待ち時間はそのまま持ち（依存先を待っている
ジョブの待ち時間は 0 で、依存先が終わった時点から数えます）、起動できるようになった時点でまとめに
加わります。そのため、一時停止中のグループに置かれた古いジョブが、そのユーザーの最新のジョブを他の
ユーザーのジョブより前に押し上げることはありません。それでも並ぶ場合は、ユーザーの順序、次に id で決まります。**これは pueue からの変更点です。** pueue では
`-o` の優先度が高いジョブが他のすべてのジョブより先になります。`priority_type = "basic"` で pueue の順序
（全ジョブについて `-o` の優先度が高いもの、次に id が小さいもの。nice、サイトファクター、`top`、
フェアシェアは無視されます）に戻ります。

**即時起動。** pueue では、キュー中または stash されたジョブの `start <id>`、`add -i`、`restart -i`、
`restore -i` はジョブをすぐに起動します。gpueue でそうなるのは管理者の場合だけで、グループの `parallel`
の上限、GPU スロット、一時停止中のグループ、依存関係を無視してすぐに起動します。それ以外のユーザーでは、
これらのコマンドはジョブ（`restart -i` では新しいコピー、`restore -i` ではリストア）を、`gpueue top` と
まったく同じように自分のキュー中のジョブの先頭に置きます。その後ジョブは上の優先度で他のユーザーのジョブと
競い、グループの `parallel` の上限、GPU スロット、一時停止中のグループ、依存関係は他のキュー中のジョブと
同じく適用されます。コマンドは、ジョブが起動されたのではなく自分のジョブの先頭にキューされたことを表示し、
API の応答には `"queued": "first"` が入ります。`enable_user_top = false` のとき、または
`priority_type = "basic"`（所有者の順序が適用されない）のときは、ジョブは移動されずに他のジョブと同じように
キューされます（`"queued": "in_line"`）。一時停止中のジョブの `start` はこれまでどおり再開です。

**コマンド。**

| コマンド | 実行できるユーザー | Slurm |
| --- | --- | --- |
| `gpueue prio [IDS] [--json]` | 全員 | `sprio`：待機中の各ジョブをスケジュール順に、優先度と重み付けした要素とともに表示します（コマンドは表示しません） |
| `gpueue share [--json]` | 全員 | `sshare`：ユーザーごとのシェア、正規化したシェア、生の使用量と実効使用量、フェアシェアの要素 |
| `gpueue add --nice N`、`gpueue nice <IDS>... <N>` | 所有者（上げることのみ。0 未満にはできません）、管理者（任意の `N`） | `--nice`、`scontrol update nice=` |
| `gpueue top <IDS>...` | 所有者（`enable_user_top` のとき）、管理者 | `scontrol top` |
| `gpueue site <IDS>... <N>` | 管理者 | サイトファクター |
| `gpueue share set <USER> <N>` | 管理者 | `sacctmgr modify user set fairshare=` |
| `gpueue group priority <GROUP> <N>` | 管理者 | パーティションの `PriorityJobFactor` |

Slurm と同じく、ユーザーは nice 値を上げることしかできないため、管理者が設定した値を元に戻すことはできません。
restart したジョブ（コピーでも `restart --in-place` でも）は、0 以上の nice 値は引き継ぎますが、負の値、
サイトファクター、`top` の位置は引き継ぎません。`gpueue prio` はスケジューラーが考慮するジョブをちょうどその
順序で一覧表示します。`gpueue status --json` は、ジョブの `nice` とグループの `priority` が 0 でないときに表示します。

リリースはまだ公開されていないため、これは開発ビルドにだけ関係します。優先度より前のビルドのデータベースは、
デーモンの起動時にスキーマバージョン 6 に移行します。既存のジョブは `-o` の優先度を保ち、以後それはその
所有者のジョブの順序だけを決めます。使用量は 0 から始まります。古いビルドは移行したデータベースを開けません
（`gpueue update` はそれらのインストールを拒否します）。

**設定。** `/etc/gpueue/config.toml` の `[priority]` テーブルです（変更したら `gpueue.service` を再起動して
ください）。`[priority]` の行より下のキーはこのテーブルに属するので、他のキーはその上に置いてください。

| キー | デフォルト | Slurm |
| --- | --- | --- |
| `priority_type` | `"multifactor"`（`"basic"`：pueue の順序） | `PriorityType` |
| `priority_weight_age` | `1000` | `PriorityWeightAge` |
| `priority_weight_fairshare` | `10000` | `PriorityWeightFairshare` |
| `priority_weight_job_size` | `0` | `PriorityWeightJobSize` |
| `priority_weight_partition` | `0` | `PriorityWeightPartition` |
| `priority_weight_tres` | `""`（例：`"GRES/gpu=1000"`） | `PriorityWeightTRES` |
| `priority_max_age_secs` | `604800`（7 日） | `PriorityMaxAge` |
| `priority_decay_half_life_secs` | `604800`（7 日。0：減衰なし） | `PriorityDecayHalfLife` |
| `priority_calc_period_secs` | `300` | `PriorityCalcPeriod` |
| `priority_usage_reset_period` | `"none"`（`"now"`、`"daily"`、`"weekly"`、`"monthly"`、`"quarterly"`、`"yearly"`） | `PriorityUsageResetPeriod` |
| `priority_favor_small` | `false` | `PriorityFavorSmall` |
| `tres_billing_weights` | `"CPU=1.0,GRES/gpu=10.0"` | `TRESBillingWeights` |
| `enable_user_top` | `true` | `SchedulerParameters=enable_user_top` |

たとえば、GPU を多く使うジョブを優先し、`urgent` グループの優先度を上げるには次のようにします。

```toml
[priority]
priority_weight_tres = "GRES/gpu=2000"
priority_weight_partition = 5000
```

```sh
sudo gpueue group priority urgent 10
```

<a id="gpu-detection"></a>
## GPU の検出

デーモンは起動時に一度だけ GPU を検出します。GPU を追加、取り外し、交換したら `gpueue.service` を再起動して
ください。

- `gpu_detection = "auto"`（デフォルト）では、実行時に NVML（`libnvidia-ml.so.1`）を読み込み、`[[gpus]]` は
  無視します。NVML を読み込めないか初期化できない場合は、その理由をログに記録し（`journalctl -u
  gpueue.service`、"GPU detection with NVML failed" で始まる警告）、代わりに設定の `[[gpus]]` のリストを使います。
  リストがなければ GPU なしで動きます。
- `gpu_detection = "static"` では、常に `[[gpus]]` のリストを使います。

NVML には `/dev/nvidiactl` と `/dev/nvidia<N>` への読み書きのアクセスが必要です。これらのノードは通常モード
0666 なので、`gpueue` ユーザーに追加のグループは要りません。そのためパッケージの `gpueue.service` は
`PrivateDevices=yes` を使いません（その専用の `/dev` には NVIDIA のノードがありません）。代わりに
`DevicePolicy=closed` と、NVIDIA の制御デバイスと GPU デバイスに対する `DeviceAllow=` を使うので、デーモンが
開けるのはそれらと標準の疑似デバイスだけです。ユニットをカスタマイズする場合はこれらの行を残してください。
そうしないと NVML は "NVIDIA driver is not loaded"（ノードが隠れている）や "internal driver error"（ノードが
許可されていない）で失敗します。どちらなのかはログのメッセージで分かります。

`gpueue service install` は、設定ファイルを作成するとき、フォールバックとして `nvidia-smi` から `[[gpus]]` の
リストも書き込みます。`gpueue doctor` は、デーモンがどの GPU をどうやって検出したかを問い合わせ、`nvidia-smi`
と比較します。auto モードで NVML が失敗したとき、`nvidia-smi` には GPU があるのにデーモンにはないとき、
デーモンの GPU（または `[[gpus]]` のフォールバックのリスト）が `nvidia-smi` と異なるとき、
`gpu_detection = "static"` で `[[gpus]]` が空のときに警告します。NVML が動いている間は `[[gpus]]` が空でも
問題ありません。リストは次のようになります。

```toml
[[gpus]]
index = 0                 # PCI bus order, as nvidia-smi and CUDA_DEVICE_ORDER=PCI_BUS_ID
uuid = "GPU-..."          # nvidia-smi --query-gpu=index,uuid,name,memory.total --format=csv
name = "NVIDIA GeForce RTX 4090"
memory_total_bytes = 25757220864
```

| フィールド | デフォルト | 意味 |
| --- | --- | --- |
| `index` | 必須 | nvidia-smi の番号付けと同じ、PCI バス順での GPU の番号。一意。`gpueue add --gpu` はこれを受け取り、ジョブからは `CUDA_VISIBLE_DEVICES` で見えます（`CUDA_DEVICE_ORDER=PCI_BUS_ID` 付き）。 |
| `uuid` | `""` | GPU の UUID。ある番号で以前と異なる UUID の GPU が見つかると、デーモンは起動時に警告します。 |
| `name` | `""` | モデル。`gpueue gpu` で表示され、各チェックポイントとともに記録されます。リストアには同じモデルが必要です。 |
| `memory_total_bytes` | 未設定 | GPU のメモリ（バイト）。`gpueue gpu` で表示されます |

`gpueue gpu` はデーモンが使っているものを表示します。NVML では、各 GPU の名前、メモリ、ドライバーの
バージョンです。GPU のエンドツーエンドテストは、パッケージのユニット内での NVML による検出を確認します。

デーモンが以前は知っていたのにもう見つからない GPU は、利用不可として一覧に残り、スロットの上限も保たれます。
それを必要とするジョブはキューに留まり、それを要求する新しいジョブは拒否されます。各 GPU は 1 ジョブの上限で
始まります。`gpueue gpu parallel <N> [--gpu LIST]` で変更します（`0` は無制限）。

<a id="gpu-isolation"></a>
## GPU の分離

`CUDA_VISIBLE_DEVICES` は CUDA にどの GPU を使うかを伝えるだけで、ジョブが書き換えることもできます。
`gpu_device_isolation = true`（デフォルト）のとき、ヘルパーはさらに、ジョブの systemd ユニットの cgroup
デバイスコントローラーで、各ジョブが開けるデバイスノードを制限します。

- 開始したユニット（`gpueue-job-<id>.service`）もリストアしたユニット（`gpueue-job-<id>-r<n>.service`）も、
  すべてのジョブのユニットに `DevicePolicy=closed` が設定されます。これは標準の疑似デバイス
  （`/dev/null`、`/dev/zero`、`/dev/full`、`/dev/random`、`/dev/urandom`、`/dev/tty`、`/dev/ptmx` と
  `/dev/pts/*` の疑似端末）を許可し、リストにないほかのものはすべて拒否します。
- GPU を使うジョブには、割り当てた各 GPU の `/dev/nvidia<minor>` と、`/dev/nvidiactl`、`/dev/nvidia-uvm`、
  `/dev/nvidia-uvm-tools`、`/dev/nvidia-modeset`、`char-nvidia-uvm`（ユニファイドメモリのデバイスをメジャー
  番号で指定したもの。ドライバーがユニットの開始後にノードを作る場合に備えます）の `DeviceAllow=`
  （読み書き）が付きます。
- CPU だけのジョブは NVIDIA デバイスをまったく開けません。
- リストアしたジョブには、リストア先の GPU（チェックポイント時と同じ番号）のノードが許可されます。
  `criu restore` と、plugin モードでは CRIU の CUDA プラグインはリストアのユニットの中で動きます。manual
  モードでは、リストアされたプロセスの中でドライバーがデバイスノードを開き直します。分離が有効でも CUDA
  ジョブのチェックポイントとリストアは動作します（GPU のエンドツーエンドテストで確認しています）。

この制限は root を含むユニットのすべてのプロセスに適用されます。他の GPU の `/dev/nvidia<N>` を開くと
`EPERM` で失敗します。ジョブの中の `nvidia-smi` には、そのジョブが開ける GPU だけが表示されます。CPU だけの
ジョブでは `Failed to initialize NVML: Unknown Error` で失敗し、CUDA はデバイスを見つけません。ユニットに
何が設定されたかは次のように確認できます。

```sh
systemctl show -p DevicePolicy -p DeviceAllow gpueue-job-<id>.service
```

GPU 番号は PCI バスの順序に従いますが、デバイスノードの名前はドライバーのマイナー番号で、両者は一致すると
は限りません。ヘルパーは開始とリストアのたびに、`/proc/driver/nvidia/gpus/<PCI address>/information` の
`Device Minor` からマイナー番号を読みます。NVML や CUDA と同じく、PCI バスの順序で、`GPU Excluded: Yes` の
GPU を除いて数えます。ドライバーが知らない GPU 番号（存在しない GPU の `[[gpus]]` エントリーや、ドライバーが
読み込まれていない場合）には、その GPU 自身のノードは許可されず、ヘルパーはそのことをログに記録します。

分離を無効にするには `gpu_device_isolation = false` を設定し、`gpueue-helper.service` を再起動します。
それ以降に開始またはリストアしたジョブにはデバイスの制限がかかりません。実行中のジョブはユニットの設定を
そのまま保ちます。次の場合は無効にする必要があります。

- **MIG** を使うホスト：CUDA は MIG インスタンスの `/dev/nvidia-caps/nvidia-cap<N>` も開きますが、これは
  許可されません。
- ジョブがほかのデバイスノード（`/dev/fuse`、`/dev/kvm`、`/dev/infiniband` 以下の RDMA デバイスなど）を
  必要とする場合。

`gpueue doctor` は分離が有効かどうかを表示します（`gpu isolation`）。

<a id="checkpoint-storage"></a>
## チェックポイントの保存先

- イメージは `checkpoints_dir`（デフォルトは `/var/lib/gpueue/checkpoints/<job id>/`）に保存され、root だけが
  読めます。各ディレクトリには `images/`（CRIU のイメージファイル）、`dump.log`、`restore.log`、`meta.json`
  （GPU、ドライバー、サイズ、時刻）、最後に書き込まれる `COMPLETE` マーカーがあります。マーカーのない
  ディレクトリは不完全なもので、ヘルパーの起動時に片付けられます。ただし、ダンプが成功したもの（`DUMPED`
  マーカーがあるか、CRIU のログがそう示すもの）は例外です。そのときジョブのプロセスはもう存在せず、
  イメージだけがジョブの残りのすべてです。ヘルパーはそうしたディレクトリを起動時かジョブのリストアの前に
  仕上げ、決して削除しません。ダンプの成功直後にメタデータの書き込みが失敗した場合（ディスクがいっぱいか
  故障している場合）、ジョブはチェックポイント済みとして扱われ、ヘルパーは理由をログに書き、デーモンは警告を
  ログに書きます。メタデータを書けるようになるまでリストアは失敗しますが、イメージは残ります。
  ヘルパーはシステムのシャットダウン時の
  ダンプのあと、そこに `shutdown-manifest.json` も書き込みます。
- 各ジョブのイメージは最大 1 つです。リストアが成功したとき、ジョブが削除されたとき（`remove`、`clean`、
  `reset`）、またはダンプが失敗したときに削除されます。リストアが失敗してもイメージは残ります。
- ダンプが失敗すると、ヘルパーはその `dump.log` を同じディレクトリの `failed-logs/<job id>-<unix ms>.log`
  （root のみ）に移し、最新の 20 個を残します。ジョブの最後のエラーにそのファイル名が示されます。
- 容量：各ダンプの前に、ヘルパーは少なくとも（ジョブの RSS + VRAM + ジョブ専用の `/tmp` と `/dev/shm` に
  保存されている分）× 1.2 の空き容量を要求します。足りなければ
  チェックポイントは拒否され、ジョブは動き続けます。イメージは圧縮されません。テストでは、7 GB の VRAM を使う
  ジョブで 8.1 GB のイメージができました。tmpfs は使わないでください（`gpueue doctor` は失敗します）。空きが
  20 GiB 未満のとき、または `checkpoint_size_limit_bytes` までの残りより少ないときも警告します。
- 上限：`checkpoint_size_limit_bytes` を設定すると、合計が上限を超える原因となったチェックポイントの所有者に
  一度警告が出ます。管理者は、コマンドを実行したときに、最大で `checkpoint_size_warning_interval_secs` ごとに
  警告を受け、`gpueue doctor` もそれを報告します。ダンプは拒否されず、何も自動では削除されません。管理者は
  `gpueue status` で全ジョブのイメージのサイズを見られます。
- 別のディスクを使うには、root が所有するディレクトリ（モード 0700、すべての親が root の所有で、グループや
  その他のユーザーが書き込めない）を作り、`checkpoints_dir` を設定し、シャットダウン時にそのファイルシステムが
  アンマウントされる前にヘルパーが停止するようにします。

  ```sh
  sudo install -d -o root -g root -m 0700 /data/gpueue-checkpoints
  sudo systemctl edit gpueue-helper.service
  #   [Unit]
  #   RequiresMountsFor=/data/gpueue-checkpoints
  sudo systemctl restart gpueue-helper.service gpueue.service
  ```

  パスを変更する前に、既存のチェックポイントを移すかリストアしてください。

<a id="cuda-plugin-mode"></a>
## CUDA プラグインモード

デフォルト（`cuda_checkpoint_mode = "manual"`）では、ヘルパーが自分で cuda-checkpoint を実行し（`lock`、
`checkpoint`、続いて `criu dump`。`criu restore`、続いて `restore` と `unlock`）、CRIU には空のプラグイン
ディレクトリを指定します。plugin モードでは `criu` だけを実行し、CRIU の CUDA プラグインがダンプとリストアの
適切なタイミングで cuda-checkpoint を呼び出します。有効にするには：

1. プラグイン付きで CRIU 4.0 以降をビルドし（gpueue は 4.2.1 でテストしています）、プラグインを CRIU の組み込みの
   プラグインディレクトリ `/usr/lib/criu` にインストールします。plugin モードではヘルパーは `-L` を渡さないので、
   CRIU はそこを探します。

   ```sh
   git clone --branch v4.2.1 https://github.com/checkpoint-restore/criu && cd criu
   make -j criu cuda_plugin
   sudo install -m 0755 criu/criu /usr/local/sbin/criu
   sudo install -D -m 0755 plugins/cuda/cuda_plugin.so /usr/lib/criu/cuda_plugin.so
   ```

2. cuda-checkpoint を `/usr/local/bin`（または systemd サービスのデフォルトの `PATH` 上の別の場所）に置きます。
   プラグインは `cuda-checkpoint` を名前で実行し、`cuda_checkpoint_path` は無視します。
3. `/etc/gpueue/config.toml` で `cuda_checkpoint_mode = "plugin"` を設定し、
   `sudo systemctl restart gpueue-helper.service` を実行して、`gpueue doctor`（`cuda plugin` の行）で確認します。

各イメージは `meta.json` に自分のモードを記録し、そのモードでリストアされるので、`cuda_checkpoint_mode` を変更
しても既存のチェックポイントは使えます。plugin モードのイメージのリストアにはプラグインが必要なので、それらの
イメージをリストアするまではインストールしたままにしてください。プラグインがなければ、ヘルパーは plugin モードで
CUDA ジョブのダンプを失敗させる代わりに拒否します（ジョブは動き続けます）。CPU のみのジョブには不要です。

トレードオフ：

| | manual（デフォルト） | plugin |
| --- | --- | --- |
| CRIU | プラグインのないディストリビューションのパッケージを含む、任意の CRIU 4.2 以降 | `cuda_plugin.so` 付きでビルドした CRIU |
| リストア時の GPU | 同じモデルとドライバー。同じモデルの別のカードは再マップされます（`--device-map`） | 同じモデルとドライバー、かつ同じ GPU の UUID（CRIU 4.2.1 のプラグインは再マップできません） |
| タイムアウト | 操作全体の期限と、cuda-checkpoint の各ステップ（lock、checkpoint、restore、unlock）のタイムアウト | 操作全体の期限のみ（`criu` の中のプラグインによる cuda-checkpoint の呼び出しもカバーします） |
| 失敗時の処理 | ヘルパーが自分で行った CUDA のステップをロールバック（リストアとアンロック）します | ダンプが失敗すると CRIU のプラグインがプロセスを再開します |

ユーザーから見るとどちらのモードも同じように動作します。どちらでも、CUDA プロセスから fork された PyTorch
DataLoader のワーカーをチェックポイントできるようにしているのは fork ガードです。ガードがなければ plugin
モードでもそれらで失敗します（`handle_device_vma plugin failed`、`Can't handle non-regular mapping`）。ガードは
モードにかかわらずすべてのジョブにプリロードされます。cuda-checkpoint は停止中のプロセスに対してハングするので、
どちらのモードでも一時停止中のジョブはダンプのために再開されます。リストアはどちらのモードでも停止したまま
行われます。plugin モードでは CRIU のプラグインが CUDA の状態をリストアしてから、ヘルパーのリストアのフックが
プロセスを停止させます。manual モードでは、ヘルパーが cuda-checkpoint を実行する間、CUDA のリストア
スレッドだけを（`ptrace` で）動かします。

<a id="filesystems-and-mounts"></a>
## ファイルシステムとマウント

各ジョブはそれぞれ専用のマウント名前空間で動きます。ジョブの開始時に shim はホストのマウントをそこにコピーし、
ジョブから見えるものが CRIU がダンプしてリストア時に再作成するものと一致するように修正します。

- automount ポイント（`x-systemd.automount`、autofs の直接マップ）は、ジョブが動き出す前に一度トリガーされ、
  ジョブの名前空間は automount ポイントではなくマウントされたファイルシステムそのものを得ます。20 秒以内に
  ファイルシステムが現れない automount（応答しない NFS サーバー）はそのまま残され、ジョブはそのパスを使え
  ません。
- 名前空間はその時点から private です。管理者があとからマウントやアンマウントしたファイルシステムは、実行中の
  ジョブには現れません。それらを使うには新しいジョブを開始してください。
- `/run` と `/media` 以下のマウントと、他のマウントの下に隠れたマウントは取り除かれます。各ジョブは `/tmp` と
  `/dev/shm` に専用の tmpfs を持ち、それはチェックポイントに保存されます。
- ほかのコンテナのマウントも取り除かれます：`/var/lib/docker/` 以下のすべて（実行中の各コンテナの
  `overlay2/<id>/merged` のルートファイルシステムや `/dev/shm`）、`/var/lib/containers/`（root で動く podman と
  CRI-O）、`/var/lib/containerd/`、`/var/lib/kubelet/pods/`、`/var/snap/docker/common/var-lib-docker/`（Docker の
  snap）、`/var/lib/rancher/k3s/agent/containerd/`（k3s）です。これらはコンテナとともに現れては消えるので、
  コンテナの実行中にダンプしたジョブは、そのコンテナが終わるとリストアできませんでした（gpueue 0.1.2：`paths the
  checkpoint's mounts depend on are missing on this host:
  ["/var/lib/docker/overlay2/<id>/merged"]`。そうしたイメージにはそのコンテナのマウントが必要なので、ジョブを
  実行し直してください）。`/var/lib/docker` 自体にマウントされたファイルシステムは残ります。rootless の podman は
  コンテナ（`~/.local/share/containers` 以下）を独自のマウント名前空間にマウントするので、ジョブから見えることは
  ありません。

CRIU はリストア時に、ジョブの名前空間のすべてのマウントを、同じホストのパスをバインドマウントすることで再作成
します。そのため、**ジョブの開始時にマウントされていたすべてのファイルシステムが、同じパスに同じタイプで
再びマウントされていなければ、ジョブはリストアできません**。ジョブがそれを一度も使っていなくても同じです。
ファイルシステムは新しいマウント（別のデバイス番号、新しいループデバイス、新しい NFS マウント）として戻って
きてもかまいません。ダンプ時に、ヘルパーはこれらの各パスのファイルシステムのタイプとマウントポイント、および
ジョブが開いているファイルと作業ディレクトリを記録します。リストアの前に、それらがまだあることを（途中の
automount ポイントをトリガーしながら）確認し、なければ分かりやすいエラーで拒否します（[リストアの失敗](#restore-failures)
を参照）。イメージは残るので、ファイルシステムをマウントしてもう一度 `gpueue restore` を実行できます。
例外は、ジョブのダンプ時に既になくなっている、または応答しない FUSE やネットワークのファイルシステムです。
それを使っていないジョブは、それを除いてダンプされます（[応答しなくなったネットワークと FUSE のファイルシステム](#network-and-fuse-filesystems-that-stop-answering)
を参照）。

リストアされたジョブが動き出す前に、ヘルパーは、その所有者がジョブの各ファイルを、ジョブが開いているのと同じ
方法で、同じパスから、シンボリックリンクなしで開けることを確認します（[リストアとパス](architecture.md#restores-and-paths)
を参照）。ダンプ時にジョブが既に開いていたファイルは、今は所有者がその方法で開けなくても（ジョブが読み取り
専用で作成したファイルや、すべてのファイルが root のものとして見える sshfs マウント。ダンプの直前に開いた
ファイルも含みます）、それがまさにそのファイル
であれば通ります。

- 同じマウント済みのファイルシステム上（間に再起動や再マウントがない）：同じデバイスと inode であること。または
- ファイルシステムが再びマウントされたあと：ダンプ直後と同じ所有者、グループ、モード、サイズ、更新時刻、
  変更時刻であること。デバイス番号は新しいマウントで変わり、inode 番号はすべてのファイルシステムで安定している
  わけではありません。sshfs のような FUSE ファイルシステムは inode を検索された順に番号付けするので、再起動後は
  ジョブのファイルは通常別の番号になります（そして別のファイルが古い番号を持つこともあります）。変更時刻は
  カーネル（sshfs ではサーバー）が設定するもので、ジョブの所有者は自分のものでないファイルにそれを設定でき
  ません。

ですから、チェックポイントしたジョブのファイルには、リストアするまで触れないでください。時刻が変わった
ファイルは、他のファイルと同じように所有者の権限で確認され、ファイルを root のものとして見せる sshfs マウント
（`root@server:`、`allow_other,default_permissions`）ではリストアが拒否されます。所有者がジョブと同じように
ファイルを開けるファイルシステム（通常の場合）では、変更は問題になりません。

擬似的な再起動をまたいで（`scripts/e2e-test.sh`、シナリオ 15、16、20。21 と 22 は同じブート中）、ジョブの作業ディレクトリと開いている
ファイルを次のそれぞれに置いてテストしました。

| マウント | 再起動後のリストア |
|---|---|
| 固定のパスにマウントした別のローカルファイルシステム（ループデバイス上の ext4）。再起動後は別のデバイス | 動作する |
| バインドマウント（別のファイルシステムからの `/home/<user>`）、読み取り専用のバインドマウント、private な伝播のバインドマウント、shared な伝播の `/`（systemd のデフォルト） | 動作する |
| overlayfs | 動作する |
| `x-systemd.automount`（トリガーの前と後に開始したジョブ。リストア時にはまだトリガーされていない） | 動作する：リストアの事前チェックがトリガーする |
| FUSE のネットワークファイルシステム（ファイルが root のものとして見えるように、root が `allow_other,default_permissions` でマウントした sshfs）。ジョブがそこで自分のファイルを書き込み・リネームし、再起動後は inode 番号が変わる | リストアの前に FUSE デーモンが再び動いていれば動作する |
| 再起動後にマウントされていないファイルシステム（fstab のエントリーがない、NFS サーバーが停止している） | 拒否：`<path> needs <type> on <mount point>` |
| ジョブの実行中にデーモンが死んだ FUSE ファイルシステム（sshfs を kill：マウントは残るが応答しない。その後ホストでアンマウント）と、それを使っていないジョブ | 動作する：それを除いてチェックポイントされ（同じブート中でも）、ファイルシステムが再びマウントされるまではそこに空のアクセスできないディレクトリを、その後はファイルシステムそのものを得る |
| 同上で、ジョブの作業ディレクトリがその上にある | チェックポイントされない（シャットダウン時も）：`the job depends on a filesystem that is gone, does not answer or root cannot access: /rnfs (fuse.sshfs) does not answer (...); the job uses it: the working directory /rnfs` |
| ジョブの所有者が `allow_other` なしでマウントした FUSE ファイルシステム（sshfs）と、それを使っていないジョブ | 動作する：それを除いてチェックポイントされ、そこに空のアクセスできないディレクトリを得る |
| 同上で、ジョブの作業ディレクトリがその上にある | チェックポイントされない：`... /home/alice/pfuse (fuse.sshfs) cannot be accessed by root (a FUSE mount without `allow_other`); the job uses it: the working directory /home/alice/pfuse` |
| ホストの `/tmp` の下にマウントされた FUSE ファイルシステム（AppImage のもの。例：`/tmp/.mount_*`）。`/tmp` が普通のディレクトリのときのユーザーのディレクトリの中のもの（`/tmp/alice-dir/fuse`）も | 動作する：ジョブ専用の `/tmp` がそれを覆うので、ジョブの名前空間にはそもそも含まれない |
| ジョブの所有者が自分のディレクトリに `allow_other` でマウントした FUSE ファイルシステム（`/home/alice/a22/afuse`）と、それを使っていないジョブ | 動作する：それを除いてチェックポイントされ、そこに空のアクセスできないディレクトリを得る（CRIU はホストのパスを名前で再びバインドするが、所有者はそれを差し替えられる） |
| 同上で、ジョブの作業ディレクトリがその上にある | チェックポイントされない：`... /home/alice/a22/afuse (fuse.sshfs) could only be restored by binding a host path that someone other than root can redirect: /home/alice/a22/afuse (/home/alice is not a directory that only root or a system account can change); the job uses it: ...` |
| systemd-resolved のソケットと `resolv.conf` ファイル。shim が `/run/systemd/resolve`（システムユーザー `systemd-resolve` の所有、モード 0755）から各ジョブの `/run/systemd` にバインドする | 動作する：チェックポイントされ、リストア時にホストから再びバインドされる（gpueue 0.1.2 はこれらを除いてダンプし、その後すべてのリストアを `the mount points [...] of filesystems that were left out of the checkpoint ... are missing on this host` で拒否していた。そうしたイメージは、ヘルパーを更新すればホストのファイルとともにリストアされる） |
| 管理者がマウントした tmpfs（例：`/scratch`）にジョブのファイルや作業ディレクトリがある | 拒否：再起動後は中身が消えています。tmpfs は再起動を生き延びる必要のないジョブにだけ使ってください。同じブート中のリストアは動作します。 |

テストしていないもの、または対応していないもの：

- **カーネルの NFS**（NFSv3/v4）はテストしていません。Docker のテスト環境ではホストの `nfs` モジュールを読み
  込めないためです。上記のチェックでは、NFS は他のファイルシステムと同じように扱われます。ヘルパーの開始前に
  マウントされ、シャットダウン時にヘルパーがジョブをダンプしたあとでアンマウントされるように、`/etc/fstab`
  （`_netdev` または `x-systemd.automount`）からマウントしてください（`gpueue-helper.service` は
  `remote-fs.target`、`network.target`、`network-online.target` のあとに順序付けられています）。ジョブの
  ユニットも `remote-fs.target` と `network.target` のあとに順序付けられている（順序のみ）ので、ヘルパーの
  ダンプのあとも動いているジョブ（`--no-checkpoint` のジョブと、ダンプが失敗または拒否されたジョブ）は、NFS が
  アンマウントされネットワークが止まる前に SIGTERM を受け取り、`grace_secs` の間に作業を保存できます。
  ジョブが開いていたファイルはパスで開き直されるので、サーバー上にまだ存在している必要があります。
- **autofs の間接マップ**（autofs デーモンが `/net/<host>` や `/home/<user>` を必要に応じてマウントするもの）：
  ジョブの開始時にマウントされていたキーはジョブから見えますが、あとで初めてアクセスされたキーは見えません
  （名前空間は private です）。そうした名前空間のリストアはテストしていません。
- **リムーバブルメディアと `/run`：** ジョブからはまったく見えません。
- 再起動後に同じパスに再びマウントできないファイルシステム（ブート ID やランダムな名前を含むパス）は、それが
  マウントされていた間に開始したすべてのジョブをリストア不能にします。

<a id="network-and-fuse-filesystems-that-stop-answering"></a>
### 応答しなくなったネットワークと FUSE のファイルシステム

ジョブの名前空間は、ジョブの開始時にマウントされていたすべての FUSE とネットワークのファイルシステムを、
ジョブが使うかどうかにかかわらず含みます。CRIU はそうしたマウントを、リストア時にホストのマウントを再び
バインドすることでしか保存できません。ジョブの実行中にそれがなくなると、ヘルパーは各ダンプの前にそれに気付き
ます。FUSE デーモンが終了した場合（SSH 接続が切れた sshfs はホスト上の自分のマウントをアンマウントしますが、
各ジョブには死んだコピーが残ります）や、応答しない場合（サーバーが停止している）です。FUSE やネットワークの
マウント（`fuse.*`、`nfs`、`nfs4`、`cifs`、`smb3`、`9p`、`ceph`、`glusterfs` など）は、ホストにもう
ないか、その上の `statfs` が接続エラー（`ENOTCONN`、`EIO`、`ESTALE` など）で失敗するか、5 秒以内に戻らない
（応答しない NFS サーバーがヘルパーを止めないよう、子プロセスで確認します）と、死んだものとみなされます。
root を拒否する FUSE マウントも同じように扱われます。ユーザーが `allow_other` なしでマウントしたもの（ユーザー
自身の sshfs）は応答しますが、そのユーザーにだけなので、（root として
動く）CRIU はリストア時にそれを再びバインドできません。root 以外が変更できるディレクトリの下にあるホストの
パスからしか CRIU が再びバインドできないマウント（タイプを問わない）も同じです。たとえばユーザーが自分の
ホームに作った `allow_other` の FUSE マウントです。CRIU はリストア時にそのパスを root としてシンボリック
リンクをたどって探すので、ユーザーはそれを本来は届かないディレクトリに差し替えられます（[リストアとパス](architecture.md#restores-and-paths)
を参照）。チェックポイントをまたいでジョブに残したいファイルシステムは、`/mnt` の下など、親ディレクトリを root
しか変更できない場所にマウントしてください。

- 死んだマウントを使っているジョブ（そこで開いているファイル、作業ディレクトリやルートディレクトリ、実行
  ファイル、マップしているファイル、そこにバインドされた Unix ソケットがある）は**チェックポイントされません**。
  ダンプはジョブに触れる前に拒否され（[チェックポイントの失敗](#checkpoint-failures) を参照）、理由には
  マウントとジョブがそこに持っているものが示されます。例：`the job depends on a filesystem that is gone,
  does not answer or root cannot access: /nfs (fuse.sshfs) is no longer mounted on the host; the job
  uses it: the working directory /nfs/run`。ファイルシステムを再びマウントして（root がアクセスできない場合は
  `allow_other` を付けて）から、チェックポイントしてください。シャットダウン時に
  は、そうしたジョブは systemd に停止され、その理由とともに `Checkpoint failed` と表示されます。
- **それ以外のジョブは、そのマウントを除いてチェックポイントされます**。リストア時、そのマウントポイントには、
  同じタイプのファイルシステムがそこにマウントされていて応答すればそのファイルシステムが、そうでなければ
  ジョブが開けない空のディレクトリ（root のもの、モード 0、`/var/lib/gpueue/checkpoints/detached-mount`）が
  与えられます。マウントポイントへの経路がジョブの所有者が変更できるディレクトリ（root でも `job_min_uid`
  未満のシステムアカウントでもない所有者のもの、またはグループやその他のユーザーが書き込めるもの）を通る場合は
  常に後者です。ファイルやソケットのバインドマウントには、代わりにジョブが開けない空のファイル
  （`/var/lib/gpueue/checkpoints/detached-mount-file`）が与えられます。ジョブがホスト上でマウントポイントの下にあるものを見ることはありません。死んだマウントの下の
  マウントは除かれます。リストアにはマウントポイント自体（ディレクトリ、ファイルやソケットのマウントでは
  ファイル）が必要です。それがなくなっている場合（ブート時に作られるマウントポイント）、再び存在するまで
  リストアは拒否されます。ジョブ自身の tmpfs（shim の `/run/systemd`、`/tmp`、`/dev/shm`、隠すディレクトリ）
  の中にあるマウントポイントはイメージからリストアされるので、ホストになくてもかまいません。

ヘルパーは除いた各マウントをログに記録します（`journalctl -u gpueue-helper`：`job N: /nfs (fuse.sshfs) is no
longer mounted on the host; the job does not use it, so it is checkpointed without it`）。

シャットダウン時、`gpueue-helper.service` は `remote-fs.target`、`network-online.target`、`network.target`
より先に停止し（ジョブをダンプし）ます。そのため `/etc/fstab` のマウントとネットワークはダンプが終わるまで
残ります。**ログインセッション**の中で動く FUSE デーモンは別です。ユーザーが SSH 経由で開始した sshfs や、
ローカルの sshfs を提供する `sftp-server` は、シャットダウンの最初にサービスと並行して、セッション
（`session-<n>.scope`）とともに停止されます。サービスは、logind がログインごとに作るユニットに対して自分を
順序付けることはできませんが、すべてのセッションスコープ向けのドロップインで、ヘルパーがジョブをダンプし
終えてから systemd がセッションを終わらせるようにできます。

```ini
# /etc/systemd/system/session-.scope.d/50-gpueue.conf
[Unit]
Before=gpueue-helper.service
```

`systemctl daemon-reload` を実行し、`systemctl show -p Before session-<n>.scope` でセッションを確認して
ください。シャットダウンは、ユーザーのプロセスを止める前にダンプを待つようになります。ユーザーの
`systemd --user` サービスのプロセスは `user@<uid>.service` で動き、同時に停止されます。
`/etc/systemd/system/user@.service.d/50-gpueue.conf` に同じ行を書けば、それらも対象になります。gpueue は
これらのドロップインをインストールしません。ドロップインがなければ、そうしたマウントのデーモンが先に
なくなったときに、それを使うジョブはチェックポイントされず、他のジョブはされます。

<a id="upgrading"></a>
## アップグレード

更新の方法は gpueue のインストール方法によります（`gpueue doctor` が "install method" として表示し、その
"update" の行で新しいリリースがあるかどうかが分かります）。

| インストール方法 | 更新方法 |
| --- | --- |
| `install.sh`（またはリリースの `gpueue service install`） | `sudo gpueue update` |
| `.deb` | `sudo apt update && sudo apt upgrade`（またはリリースの新しい `.deb` を `sudo apt install ./gpueue_<new version>_<arch>.deb` でインストール） |
| `.rpm` | `sudo dnf upgrade <URL of gpueue-<new version>-1.<arch>.rpm>`（dnf リポジトリはまだありません） |
| ソースからビルド | `cargo build --release -p gpueue -p gpueue-helper` のあと `sudo target/release/gpueue service install` |
| Nix ビルドからの `gpueue service install`（NixOS 以外） | 同じ `nix build` と `service install` をもう一度（[Nix でのインストール](#installing-with-nix) を参照） |
| AUR パッケージ `gpueue-bin`（公開後。それまで Arch は `install.sh` と `sudo gpueue update` を使います） | `yay -Syu`（任意の AUR ヘルパー）のあと `sudo systemctl restart gpueue-helper.service gpueue.service`。`gpueue update` は拒否します |
| NixOS（`services.gpueue`） | `nix flake update gpueue && sudo nixos-rebuild switch`（[NixOS](#nixos) を参照）。`gpueue update` は拒否します |
| その他のディストリビューションのパッケージ | パッケージマネージャー（[インストール方法](#install-method) を参照）。`gpueue update` は拒否し、使うべきコマンドを示します |

<a id="gpueue-update"></a>
### gpueue update

```sh
gpueue update --check          # is there a newer release? (any user; without root, update only checks)
sudo gpueue update             # install the latest release
sudo gpueue update --version 0.2.0 --yes
```

`gpueue update` に GitHub のトークンは不要です。`install.sh` と同様に、与えられればそれを使います（API の
レート制限が上がります）。`GH_TOKEN` か `GITHUB_TOKEN`（`sudo GH_TOKEN=... gpueue update`）、または
`install.sh --save-token` が書き込む `/etc/gpueue/github-token`（root、0600）です。その後、次のことを行います。

1. gpueue が `gpueue service install` でインストールされたこと（パッケージ、Nix、手動ではなく、両方のユニットが
   それに属していること。別の方法でインストールされたヘルパーのユニットは何もダウンロードする前に拒否し、
   ヘルパーのユニットがなければインストールが書き込みます）と、インストールされたバイナリ（`gpueue.service` の
   `ExecStart=` にあるディレクトリ）
   として実行されていることを確認します。
2. 最新のリリース（または `--version`）を調べ、自分のバージョンと比較します。最新であれば何もしません。
   ただし、インストールされた `gpueue-helper` がない、または別のバージョンを報告する場合（以前の更新が中断
   された場合）は、組を修復するためにリリースをもう一度インストールします。古いバージョンは `--version` で
   指定されたときだけインストールします。
3. 確認を求めます（`-y/--yes` で省略できます）。
4. tarball、`.sha256`、`.minisig` を `/var/lib/gpueue/update`（root、0700）にダウンロードし、`gpueue` に組み込まれた
   リリース鍵で minisign の署名を（`install.sh` と同様に、その trusted comment がそのリリースの
   `gpueue <tag> <file>` であることも）、そしてチェックサムを確認します。
5. tarball を展開し（通常ファイルとディレクトリのみ）、新しい `gpueue` に対応するデータベースのスキーマを
   問い合わせます。デーモンのデータベースを開けないリリース（スキーマ変更より前の古いもの）は、何かを変更する
   前に拒否されます。
6. インストール先のディレクトリに対して新しい `gpueue service install` を実行し、Nix でビルドしたインストールの
   Nix GC ルートを削除し（リリースのバイナリはストアを必要としません）、デーモンが新しいバージョンで動いて
   ヘルパーに接続するまで待ってから、ダウンロードしたものを削除します。`service install` が失敗した場合、
   `gpueue update` はインストールされたバイナリを1つずつ調べます。どちらもリリースのものになっていなければその
   エラーで止まり、片方だけなら止まってもう一度実行するよう伝え（再実行で組が修復されます）、両方なっていれば
   （最後の確認だけが失敗した場合）警告して続けます。

`install.sh`（必要なら `--version` を付けて再実行）でも同じことができます。署名は `PATH` にある `minisign`、
なければダウンロードした固定版の minisign で確認します（[インストーラーの実行](#running-the-installer)
を参照）。

`service install`（したがって上記のどちらの方法も）は、バイナリとユニットを置き換え、何か変わっていれば両方の
サービスを再起動します。両方のバイナリを先に書き込んでから、ヘルパーから順に所定の場所に置きます。片方を置けな
かった場合はもう片方を元に戻すので、失敗（たとえばディスクがいっぱい）しても、新しい `gpueue` の隣に古い
`gpueue-helper` が残ることはありません。設定ファイルは保持されます。

- **実行中のジョブには影響しません。** ジョブはそれぞれの systemd ユニット（`gpueue-job-<id>.service`、
  リストア後は `gpueue-job-<id>-r<n>.service`）で動いています。新しいデーモンはデータベースから状態を引き継ぎ、
  新しいヘルパーは実行中のジョブを再び見つけます。
- **ヘルパーの再起動：** 単なる `systemctl stop/restart gpueue-helper`（システムのシャットダウンではない）で
  何かがチェックポイントされることはありません。停止するヘルパーは新しい起動、リストア、チェックポイントを拒否し、
  処理中の操作を先に終わらせます。処理中のダンプには時間がかかることがあり、2 回目の SIGTERM ですぐに終了させられます。
  拒否されたり中断されたりした起動はデーモンが再びキューに入れ、ヘルパーが戻ってきたら再試行します。拒否された
  `checkpoint` は "helper unavailable" で失敗し、ジョブは実行中（または一時停止中）のまま残ります。ヘルパーが
  停止している間、`pause`、`kill`、`checkpoint`、一時停止中のジョブの再開は "helper unavailable" で失敗します。
  `add -i`、`start`、`restore -i`、`restart -i` は受け付けられ、ヘルパーが再接続したときにジョブが起動されます。
- **バージョン：** CLI とデーモンは同じバージョンでなければなりません。バージョンの合わないクライアントには、
  どちら側を更新すべきかを示すエラーが返り、`gpueue doctor` もそれを警告します。ユーザーが実行する `gpueue`
  バイナリも同時に更新してください。
- **チェックポイントはアップグレードを越えて使えます。** ジョブのプロセスツリーにはヘルパーのバイナリが含まれ
  （名前空間の init は `gpueue-helper shim-init` です）、CRIU は実行ファイルがダンプ時と同じファイルである場合に
  限りプロセスをリストアします。アップグレードは `/usr/bin/gpueue-helper`（または
  `/usr/local/bin/gpueue-helper`）を置き換えるので、ジョブのユニットはそのファイルを実行しません。ヘルパーは
  起動時に自分自身を `/var/lib/gpueue/lib/gpueue-helper-<内容のハッシュ>`（root、0755）にコピーし、新しい
  ジョブのユニットはそのコピーを実行します（`journalctl -u gpueue-helper` に `job units run ...` と表示されます）。
  ジョブは自分を起動したコピーを、実行中もチェックポイントとリストアを経ても使い続け、アップグレードは新しい
  ジョブのために新しいコピーを追加します。ヘルパーは起動時に、ジョブの記録からもチェックポイントからも必要と
  されなくなったコピーを削除するので、コピーが 1 つか 2 つ（まだ使われているバージョンごとに 1 つ）あるのは
  正常です。手で削除しないでください。コピーがなくなったチェックポイントはリストアできません。SELinux のある
  システム（Fedora、RHEL）では、コピーには `/var/lib` の他のファイルの `var_lib_t`（systemd が実行できないこと
  があります）ではなく、インストールされたバイナリのラベル（`bin_t`）が付きます。ヘルパーは起動のたびと、ジョブの
  ユニットがコピーを実行する前にそれを付け直すので、再ラベル付け（`restorecon -R /var/lib`）をしても問題ありません。
  NixOS ではコピーは作られず、Nix ストアが古いバイナリを保持します（[NixOS](#nixos) を参照）。
- **0.1.1 以前からのアップグレード：** これらのバージョンが起動したジョブは、インストールされたバイナリそのものを
  実行しており、アップグレードはそれを置き換えます。チェックポイント済みのジョブはアップグレードの前にリストア
  してください。アップグレード後はリストアが失敗します（`restore.log` に `File usr/bin/gpueue-helper has bad
  size`）。実行中のジョブは動き続けますが、チェックポイントはできなくなり（`Can't dump ghost file
  /usr/bin/gpueue-helper`）、シャットダウン時も同じなので、次の再起動の前に終わらせてください。アップグレード後に
  起動したジョブには影響しません。
- **0.1.2 以前が起動したジョブ**では、シェルが exec するプログラム（`exec python3 train.py` のような
  コマンド。`/bin/sh` が bash のシステム（Fedora、RHEL、Arch）では `python3 train.py` のように単一の
  プログラムだけのコマンドも）で、SIGTERM、SIGINT、SIGHUP など shim が転送するシグナルがブロックされています。そのような
  プログラムはこれらのハンドラーを実行しません。`gpueue kill` や、チェックポイントされないジョブが
  シャットダウン時に受け取る SIGTERM では終わらず、`grace_secs` の後の SIGKILL で終了します。シェルが
  fork して実行するプログラムには影響しません。ブロックされたシグナルはアップグレード後も残り、ジョブを
  チェックポイントしてリストアしても残ります（CRIU が復元するため）。シグナルハンドラーに頼るジョブ（自分で
  チェックポイントを保存するものなど）は再起動してください。
- `/var/lib/gpueue/lib` の古い fork ガードのライブラリは意図的に残されます。アップグレード前にチェックポイント
  したジョブのリストアには、まさにそのファイルが必要だからです。
- NVIDIA ドライバーをアップグレードする前に、チェックポイント済みのジョブをリストアするか削除してください。
  異なるドライバーバージョンでのリストアは拒否されます。

<a id="uninstalling"></a>
## アンインストール

NixOS では、代わりに `services.gpueue.enable = false` を設定して `sudo nixos-rebuild switch` を実行します
（[NixOS での更新と削除](#updating-and-removing-on-nixos) を参照）。

`.deb` には `sudo apt remove gpueue` か `sudo apt purge gpueue`（[Debian/Ubuntu パッケージのインストール](#installing-the-debianubuntu-package)
を参照）、`.rpm` には `sudo dnf remove gpueue`（[Fedora/RHEL/Rocky パッケージのインストール](#installing-the-fedorarhelrocky-package)
を参照）、AUR パッケージ（公開後）には `sudo pacman -R gpueue-bin`（[Arch Linux (AUR)](#arch-linux-aur) を参照）を
使います。当面の Arch Linux と、`install.sh` や `gpueue service install` によるインストールでは次のようにします。

```sh
sudo gpueue uninstall            # stop, disable and remove the units, snippets and copied binaries
sudo gpueue uninstall --purge    # also delete /etc/gpueue, /var/lib/gpueue, /run/gpueue, /run/gpueue-helper
```

`--purge` は `config.toml` がデータの置き場所として設定したディレクトリ（`data_dir`、`helper_state_dir`、
`logs_dir`、`checkpoints_dir`）も、上のディレクトリの外にあれば削除します。`config.toml` がヘルパーの所有者の
確認に失敗するか解析できない場合、または設定されたディレクトリが `/` であるか、上のディレクトリを含む（`data_dir = "/var/lib"`）か、ヘルパーの確認に失敗する場合
（そのディレクトリかその上のディレクトリが root の所有でない、またはグループやその他から書き込める）は、
何も変更する前に拒否します。削除するディレクトリの外にある `state_db` は、デーモンユーザーのディレクトリに
あるので残して報告します。

`gpueue service uninstall` は同じコマンドで、`install.sh --uninstall [--purge] [--yes]` はこれを実行します。
`--purge` は端末で確認を求めます。端末がない場合は `--yes` を渡してください。

- 削除されるのは、インストールのマーカーで始まるファイルだけです。それ以外は報告されて残ります。
- バイナリが削除されるのは、`/var/lib/gpueue/install-receipt` に `gpueue service install` がコピーしたものとして
  記載されている場合だけです（記録されたディレクトリの `gpueue` と `gpueue-helper` のみ）。`/usr/bin` などで
  その場で使われたバイナリは残ります。レシートができる前に行ったインストールではバイナリは残り、`uninstall` は
  手動で削除すべきものを表示します。使えないレシートの場合も同様です。記載されたバイナリのうち、今では
  パッケージが所有しているもの（dpkg、rpm、pacman に問い合わせます）は残り、パッケージマネージャーが所有者を
  判断できなかったものも残ります。Nix の GC ルートとレシートも削除されます。削除できなかったバイナリがあれば、
  それを記載したままレシートは残ります（あとで `sudo gpueue uninstall` を再実行してください）。
- インストール済みの `gpueue` が `gpueue uninstall` より古い場合、`install.sh --uninstall` はその
  `service uninstall` を実行してから、ユニットのディレクトリにある `gpueue` と `gpueue-helper` を削除します。
- 実行中のジョブはそれぞれのユニットで動き続けます。`uninstall` はそれについて警告し、`--purge` は
  `gpueue-job-*` ユニットがアクティブな間と、systemd がそれらを列挙できないときは実行を拒否します。先に
  `sudo systemctl stop 'gpueue-job-*'` で停止
  するか、アンインストールの前にチェックポイントか kill してください。
- `--purge` は確認を求めます（`-y/--yes` で省略できます）。マウントポイント（たとえば `/run/gpueue-helper` 以下の
  `/` のバインドマウント）の中には降りていきません。それらは報告されるので、手動でアンマウントして削除して
  ください。削除するディレクトリ自体がマウントポイントの場合（たとえば独立したファイルシステム上の
  `/var/lib/gpueue`）は、中身を削除してディレクトリは残します。削除しようとしたときに空でなくなっていた
  ディレクトリ（動いているプロセスが書き込み続けるユーザーのログディレクトリ）も残して報告し、ほかの
  ディレクトリの削除は続けます。削除はシンボリックリンクを決してたどりません
  （ユーザーが自分のログディレクトリに置いたものも含みます）。
- システムユーザー `gpueue` とグループ `gpueue-admin` は常に残ります。

<a id="troubleshooting"></a>
## トラブルシューティング

<a id="where-to-look"></a>
### どこを見るか

```sh
journalctl -u gpueue -u gpueue-helper          # daemon and helper
journalctl -u 'gpueue-job-*'                  # job units (start/stop, OOM kills)
gpueue status; gpueue log <id>                 # job state, last error, output
sudo ls /var/lib/gpueue/checkpoints/<id>/     # dump.log, restore.log (CRIU logs, root only)
sudo ls /var/lib/gpueue/checkpoints/failed-logs/   # dump.log of failed dumps (newest 20)
```

ジョブの出力は `/var/lib/gpueue/logs/<uid>/<id>.log` に書き込まれます。チェックポイントやリストアが失敗すると、
ジョブの最後のエラー（所有者と管理者には `gpueue status` と `gpueue log` のヘッダーに表示されます）に CRIU の
ログの短い要約が含まれます。ダンプが失敗した場合は、完全なログの場所も示されます。例：
`(CRIU log: /var/lib/gpueue/checkpoints/failed-logs/12-1760000000000.log)`。デーモンは `info` レベルでログを
出力します。詳しく見るには、`gpueue.service` のドロップインで `Environment=RUST_LOG=debug` を設定してください。

デーモンが動いていなければ、すべてのコマンドが "cannot connect to the daemon socket" で失敗します。
`gpueue service status` と `systemctl status gpueue` を確認してください。`gpueue shutdown` のあとは、
`sudo systemctl start gpueue` を実行するか次に起動するまで、デーモンは停止したままです。

<a id="common-gpueue-doctor-failures"></a>
### よくある `gpueue doctor` の失敗

| チェック | メッセージ | 対処 |
| --- | --- | --- |
| `config` | not owned by root or group/world-writable | `sudo chown root:root /etc/gpueue/config.toml /etc/gpueue; sudo chmod go-w /etc/gpueue/config.toml /etc/gpueue` |
| `config` | invalid configuration | TOML を直してください。この状態ではデーモンは起動しません。 |
| `config` | not readable by others / not searchable by others (WARN) | `sudo chmod 755 /etc/gpueue; sudo chmod 644 /etc/gpueue/config.toml`：デーモンは `gpueue` として動きます |
| `criu` | not found, or too old | CRIU 4.2 以降をインストールするか、`criu_path` を設定します |
| `criu`、`cuda-checkpoint`、`gpus` | was not run as root（root 以外が置き換えられる、または設定ファイルが所有者の確認に失敗した） | プログラムとそのディレクトリを root の所有にし、グループやその他のユーザーが書き込めないようにするか、先に `config` の問題を直します |
| `cuda-checkpoint` | not found, or lacks `--action lock/checkpoint/restore/unlock` | r570 以降のドライバー用のバイナリを `/usr/local/bin` にインストールします |
| `cuda-checkpoint` | version X but the driver is Y | ドライバーのメジャーバージョンに合った cuda-checkpoint を使います |
| `cuda plugin` | `cuda_checkpoint_mode` is `"plugin"` but `/usr/lib/criu/cuda_plugin.so` is missing | プラグインをインストールする（[CUDA プラグインモード](#cuda-plugin-mode) を参照）か、manual モードを使います |
| `gpus` | nvidia-smi lists N GPU(s), the configuration M | `[[gpus]]` を更新します（[GPU の検出](#gpu-detection) を参照） |
| `kernel` | lacks `/proc/sys/kernel/ns_last_pid` | `CONFIG_CHECKPOINT_RESTORE=y` のカーネルを使います |
| `update` | gpueue X is available (INFO) | `sudo gpueue update`。`update_check = false` でチェックを無効にできます |
| `install method` | mixed: two install methods at once | `fix:` の行のとおり、1 つを残して削除します（例：パッケージを残すなら `sudo gpueue uninstall`） |
| `install method` | WARN: a unit no package owns, a leftover unit, or units installed in different ways | `fix:` の行のとおり、所有者のない `/usr/lib` のユニットを削除する（`sudo rm ...`）か、両方のユニットを所有すべきインストールを再実行します |
| `gpueue.service`、`gpueue-helper.service` | not installed / not active | `sudo gpueue service install`、または `sudo systemctl start <unit>` を実行して `journalctl -u <unit>` を読みます |
| `helper binary` | the binary or a directory above it is not owned by root or is writable by others | `sudo chown root:root <path>; sudo chmod 755 <path>`（とそのディレクトリ）、または別のディレクトリに再インストールします |
| `sudo PATH` | sudo's secure_path does not include /usr/local/bin (INFO, RHEL family) | `sudo /usr/local/bin/gpueue ...`、または `sudo visudo` で `secure_path` に `/usr/local/bin` を追加します |
| `daemon write paths` | cannot write to a configured directory: not below its ReadWritePaths= | `gpueue.service` に `ReadWritePaths=` のドロップインを追加します（[インストール](#installing) を参照） |
| `helper` | the daemon is not connected to the helper | `journalctl -u gpueue-helper`。設定やデータディレクトリの所有者の問題であることが多いです。 |
| `daemon` | runs version X while this gpueue is Y | アップグレード後に `sudo systemctl restart gpueue-helper.service gpueue.service` を実行するか、CLI を更新します |
| `checkpoint dir` | on tmpfs, or little free space | `checkpoints_dir` を、ジョブの RAM + VRAM を収められるディスクに移します |
| `helper settings` | ExecStart= passes flags | 設定を `config.toml` に移し、フラグを削除します |
| `job d-bus` | allowed (`job_system_dbus = true` or `job_systemd_varlink = true`): jobs can ask systemd and polkit for actions (WARN) | ジョブに必要でなければ、そのキーを `/etc/gpueue/config.toml` から削除して `sudo systemctl restart gpueue-helper.service` を実行します |
| `job sockets` | a default of `job_hidden_socket_dirs` is left out (`/run/libvirt not hidden`), an entry is not a directory (every job fails to start), or nix-daemon trusts users or groups besides root while `/nix/var/nix/daemon-socket` is not hidden (WARN) | `job_hidden_socket_dirs` をヒントが示すリストにして（既定に戻すならキーを削除して）、`sudo systemctl restart gpueue-helper.service` を実行します |
| `orphaned jobs` | the helper runs jobs the daemon does not account for (WARN; their GPUs stay taken) | `gpueue status` は管理者にそれらを表示します。管理者は `gpueue kill <id>` で停止できます |

NixOS（`/etc/os-release` が `ID=nixos`）では、`fix:` の行は NixOS では当てはまらない手順の代わりに
[モジュールのオプション](#nixos) を挙げ、いずれも `sudo nixos-rebuild switch` を続けます。
`/etc/gpueue/config.toml` を編集する代わりに `services.gpueue.settings`（ファイルはモジュールが書き直します）、
CRIU や cuda-checkpoint を `/usr/local/bin` にインストールする代わりに `services.gpueue.criuPackage` と
`services.gpueue.cudaCheckpointPackage`、nvidia-smi と NVML には `services.gpueue.nvidiaSmiPath` と
`services.gpueue.nvmlLibrary`、`sudo gpueue service install` や `systemctl enable` の代わりに
`services.gpueue.enable = true`、`usermod` の代わりに `services.gpueue.admins`、`systemctl edit` の
ドロップインの代わりに `systemd.services.gpueue.serviceConfig.ReadWritePaths` です。

<a id="checkpoint-failures"></a>
### チェックポイントの失敗

ヘルパーはダンプの前にジョブを確認します。チェックが失敗すると、ダンプは**拒否**されます。ジョブには触れず、
ジョブは動き続け、`gpueue checkpoint` は `checkpoint refused: <reason>; the job keeps running` で失敗し、その
理由がジョブの最後のエラーになります。ダンプを行う時点になって初めて拒否された場合（たとえば
`gpueue shutdown` の最中。この場合ジョブは動いたまま残されます）、所有者は次にコマンドを実行したときに警告を
受けます。拒否の理由：

- `not enough free space for the checkpoint: need N bytes, M available`：空き容量を作るか
  `checkpoints_dir` を移してから、もう一度チェックポイントします。必要量には、ジョブが自分の `/tmp` と
  `/dev/shm` に保存している分も含まれます（その場合、メッセージにその量が示されます）。
- `the helper did not find criu when it started (install CRIU 4.2 or newer and restart
  gpueue-helper)`、`criu /usr/sbin/criu is version 3.17.1, but gpueue needs CRIU 4.2 or newer
  ...`：この CRIU ではどのジョブもチェックポイントできません（[CRIU](#criu) を参照）。CRIU 4.2 以降を
  インストールして（または `criu_path` をそれに向けて）ヘルパーを再起動してください。ヘルパーは起動時にしか
  CRIU を確認しません。
- `finding CUDA processes: the helper did not find cuda-checkpoint when it started ...`：GPU ジョブは
  cuda-checkpoint なしではチェックポイントできません。インストールして（[cuda-checkpoint](#cuda-checkpoint)
  を参照）ヘルパーを再起動してください。``... process N uses an NVIDIA device but `cuda-checkpoint
  --get-state` does not know its CUDA state: ...``：cuda-checkpoint がそのプロセスを扱えず（例えばドライバーと
  cuda-checkpoint のバージョンの不一致。`gpueue doctor` を参照）、CRIU もそのデバイスのマッピングをダンプ
  できません。
- `the job has Unix sockets bound to [...] on the host's filesystems (or to relative names ...)`：
  リストアの際に CRIU が root としてそれらのパスを作成、unlink、chown することになります。ソケットは `/tmp`
  以下の絶対パスか抽象名にバインドしてください。相対名は、バインドされたディレクトリがわからないので、
  ホストのパスとして扱われます。
- `the job has internet sockets with SO_REUSEPORT (...)`：CRIU はジョブに資格情報を戻す前に root として
  ソケットを作り直すので、リストアされた `SO_REUSEPORT` のソケットは root のものになり、同じポートの root の
  サービスのグループに加わって（その接続の一部を受け取って）しまう可能性があります。プログラムで
  `SO_REUSEPORT` を無効にするか、ジョブを `--no-checkpoint` で実行してください。
- `the job depends on a filesystem that is gone, does not answer or root cannot access: <mount point>
  (<type>) ...; the job uses it: ...`：ジョブが使っている FUSE やネットワークのファイルシステムが、ホストに
  もうマウントされていないか、応答しないか、`allow_other` なしの FUSE マウントです。再びマウントして（sshfs を
  再起動する、NFS サーバーを戻す。ユーザーの FUSE ファイルシステムは `-o allow_other` でマウントする。これには
  `/etc/fuse.conf` の `user_allow_other` が必要です）、もう一度チェックポイントしてください。[応答しなくなったネットワークと FUSE のファイルシステム](#network-and-fuse-filesystems-that-stop-answering) を参照してください。
- `the job depends on a filesystem that is gone, does not answer, root cannot access, or that CRIU
  could only restore by binding a host path that someone other than root can redirect: <mount
  point> (<type>) could only be restored by binding a host path ...`：ジョブが、ホストのパスがジョブの
  所有者の変更できるディレクトリの下にあるマウント（ユーザーが自分のホームに作った `allow_other` の FUSE
  マウント）を使っています。root と、デーモンのユーザー以外の `job_min_uid` 未満のシステムアカウントの
  ディレクトリは、グループやその他のユーザーが書き込めない限り問題ありません。チェックポイントする必要のあるジョブには、`/mnt` の下など、親ディレクトリを root しか変更
  できない場所にファイルシステムをマウントしてください。
- `the job has processes in a PID or user namespace of their own (...)`：ジョブが `unshare`、`bwrap`、
  rootless のコンテナランタイムを実行しており、CRIU は入れ子の PID 名前空間とユーザー名前空間をダンプできません。
  ジョブを `--no-checkpoint` で実行するか、入れ子の名前空間なしで実行してください。
- `the job's mounts [...] are only reachable through mounts hidden under other mounts`：ジョブのマウント
  名前空間が別のマウントの下に隠れたマウントに依存しているので、決してリストアできません。ジョブを実行し直して
  ください（`gpueue restart`）。新しいジョブには整理された名前空間が与えられます。
- `/` 自体を覆うマウントは扱えません。gpueue はそうしたダンプを拒否します。
- `<dir> is on tmpfs`：`checkpoints_dir` をディスクに移します。
- `cuda_checkpoint_mode is "plugin" but CRIU's CUDA plugin ... is missing`：プラグインをインストールする
  （[CUDA プラグインモード](#cuda-plugin-mode) を参照）か、manual モードに切り替えます。

チェックポイントされる一時停止中のジョブは、ダンプのために再開されます。ダンプが拒否されると、再び一時停止
されます。

試みたダンプが**失敗**すると、CRIU と cuda-checkpoint が既にジョブを凍結しているため、ジョブは停止されます
（SIGTERM、`grace_secs` 後に SIGKILL）。ジョブは CRIU のログの要約と完全なログのパスとともに
`Checkpoint failed` とされます。よくある理由：

- CRIU の要約に `Can't handle non-regular mapping` や `handle_device_vma plugin failed` がある：CUDA プロセスが
  `exec` せずに fork したプロセスが、まだ `/dev/nvidia*` をマップしています。通常は fork ガードがこれを防ぎ
  ます。`fork_guard` が `false` でないこと、プログラムが静的リンクでないこと、ガードがインストールされたあとに
  ジョブが開始されたことを確認してください。それでもだめなら、`spawn` か `forkserver` の開始方式を使います
  （PyTorch の DataLoader なら `multiprocessing_context="forkserver"`）。
- `checkpoint_tcp_close = false` の場合、CRIU は確立済みの TCP 接続を持つジョブのダンプを拒否します。

OS のシャットダウンや再起動の間に、ダンプが失敗したか拒否されたジョブは systemd によって停止されます。起動後は
理由とともに `Checkpoint failed` と表示され（拒否の場合は `checkpoint refused:` で始まります）、所有者に警告が
出ます。

<a id="restore-failures"></a>
### リストアの失敗

リストアが失敗してもジョブは `Checkpointed` のままで、イメージも残るので、原因を直してもう一度
`gpueue restore` を実行できます。所有者は次にコマンドを実行したときに警告を受けます。

- `gpueue refuses to run jobs as <user>: ...`：ジョブの所有者またはグループが `job_min_uid`、
  `job_denied_groups`、`job_allowed_users` によって拒否されました（[ジョブを実行できるユーザー](#who-jobs-may-run-as)
  を参照）。`the checkpointed job ran with the groups ["docker"], which are in job_denied_groups` は、
  いまは拒否されるグループを持ったままジョブがチェックポイントされたことを示します。設定を変える（そして
  ヘルパーを再起動する）か、ジョブを削除してください。

- `the checkpoint of job N was dumped but not finalized, and finalizing it failed again: ...`：ダンプは
  成功しましたが、ヘルパーがチェックポイントのメタデータを書き込めませんでした（[チェックポイントの保存先](#checkpoint-storage)
  を参照）。典型的には、`checkpoints_dir` のあるディスクがいっぱいか故障しています。それを直してから、もう一度
  リストアしてください。

- `paths the checkpoint's mounts depend on are missing on this host: [...]`：CRIU は `Can't stat mountpoint`
  や `Failed to open_tree <path>` で失敗するはずの状態です。ジョブの名前空間に、今は存在しないマウントポイント、
  またはホストのパスからのバインドマウントがありました（たとえば再起動後にマウントされていないファイル
  システム）。それをマウントするかパスを作成して、もう一度リストアしてください。
- `filesystems the checkpoint depends on are not mounted as at the dump (mount them and restore
  again): /data needs ext4 on /data (found overlay on /)`：ジョブの開始時にマウントされていたファイルシステムが
  今はマウントされていないか、そこに別のタイプのファイルシステムがマウントされています。CRIU は代わりに空の
  マウントポイントのディレクトリをバインドし、ジョブはその下のファイルシステムに書き込み続けることになります。
  マウントして（`systemctl start data.mount`、`mount -a`、NFS サーバーを起動する、など）もう一度リストアして
  ください。[ファイルシステムとマウント](#filesystems-and-mounts) を参照してください。
- `the mount points [...] of filesystems that were left out of the checkpoint (they were gone, did not
  answer or refused root at the dump) are missing on this host`：CRIU には何かをマウントするための
  マウントポイント（ディレクトリ、ファイルやソケットのバインドマウントではファイル）が必要です。
  それを作成して（またはそこにファイルシステムをマウントして）もう一度リストアしてください。ジョブ自身の
  tmpfs の中のマウントポイントは確かめません（イメージがリストアします）。gpueue 0.1.2 は systemd-resolved の
  あるホストですべてのジョブについてこれを報告していました（`/run/systemd/resolve/io.systemd.Resolve` など）。
  ヘルパーを更新して、それらのジョブをもう一度リストアしてください。
  [応答しなくなったネットワークと FUSE のファイルシステム](#network-and-fuse-filesystems-that-stop-answering) を参照してください。
- `the checkpoint binds host paths below directories that someone other than root can change,
  which CRIU would follow as root, so it is not restored: ...`：ヘルパーがダンプ時にこれを確かめるようになる
  前に作られたイメージに、ジョブの所有者が差し替えられるホストのパスからの外部マウントがあります（root と
  `job_min_uid` 未満のシステムアカウントのディレクトリは問題ありません）。安全にはリストア
  できません。削除して（`gpueue remove`）ジョブを実行し直してください。
- `files or directories the job used are missing on this host: [...]`：ジョブが開いていたファイルや作業
  ディレクトリがなくなっています。典型的には、それらが再起動後に空になる tmpfs（`/scratch`）上にあった場合です。
  ジョブはリストアできません。削除してください（`gpueue remove`）。
- `checkpoint is not compatible with this host`：GPU のモデルやドライバーのバージョンが異なるか、GPU がありま
  せん。元のドライバーと GPU でリストアしてください。plugin モードでは GPU の UUID も一致する必要があります。
  manual モードでは同じモデルの別のカードは再マップされます。
- `not enough free GPU memory`：他のジョブが GPU を使っています。待つか、GPU を空けてください。
- `the checkpointed job runs the helper binary /var/lib/gpueue/lib/gpueue-helper-<hash>, which is
  gone`：ジョブがダンプされたときのヘルパーのコピーが手で削除されています（ヘルパーはチェックポイントがある間
  それを残します）。同じビルドをそのパスに戻すか、ジョブを削除してください。
- プロセスが開いていたファイル、またはその実行ファイルやライブラリが変更または削除されています（`restore.log` を
  参照）。同じパスに元に戻してください。`File usr/bin/gpueue-helper has bad size` は、gpueue 0.1.1 以前で
  チェックポイントしたジョブをアップグレード後にリストアしようとしたことを意味します（[アップグレード](#upgrading)を
  参照）。
