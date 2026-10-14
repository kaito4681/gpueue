<a id="puzuz"></a>
# puzuz

[English](README.md) | 日本語 | [简体中文](README.zh-CN.md)

複数のユーザーで共有する 1 台の Linux マシン向けの GPU ジョブキューです。CLI は
[pueue](https://github.com/Nukesor/pueue) と同じように使えます。puzuz はその上に 2 つの機能を加えます。GPU
スロットと、[CRIU](https://criu.org) と NVIDIA [cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint)
による実行中ジョブの透過的なチェックポイント/リストア（CPU、メモリ、GPU の状態）です。マシンの再起動時にまだ
動いている学習はディスクに保存され、止まったところから再開します。最初からやり直すことはありません。

> **状態：プレリリース。** 以下の機能はすべて実装済みで、Docker 内（PID 1 の systemd、本物の CRIU、本物の
> NVIDIA GPU）でテストされています。実機での実際の再起動をまたいだテストは**まだ**行っていません。
> [プロジェクトの状況](#project-status) を参照してください。

> **リリースはまだ公開されていません。** 以下のダウンロードリンク、APT リポジトリ、`stable` ブランチは、最初の
> リリースから使えるようになります。

<a id="why"></a>
## なぜ puzuz か

共有の GPU ワークステーションでは、ジョブを `nohup` や `tmux`、ユーザーごとの pueue で動かし、GPU の取り合いを
チャットで調整するのが普通です。カーネルやドライバーの更新のための再起動は、すべての学習を止めてしまいます。
puzuz は次のものを提供します。

- **全員で 1 つのキュー**：システム全体のデーモンが全ユーザーのジョブをスケジュールし、誰がどの GPU を
  使っているかを表示します。
- **GPU スロット**：ジョブは特定の GPU を要求し（`--gpu 0,1`）、それらが空いたときにだけ開始します。
- **チェックポイント/リストア**：実行中のジョブは、OS のシャットダウン時、`puzuz shutdown` の実行時、または
  `puzuz checkpoint` で必要なときに保存されます。あとで `puzuz restore` で再開できます。コードを変更する
  必要はありません。

<a id="features"></a>
## 機能

- pueue 互換のコマンド：`add`、`status`、`log`、`follow`、`wait`、`pause`、`start`、`kill`、
  `remove`、`restart`、`clean`、`reset`、`stash`、`enqueue`、`switch`、`group`、`parallel`、
  `shutdown`、`completions`。加えて、グループ、依存関係（`--after`）、優先度、遅延実行、ラベル。
- GPU スロット：GPU ごとに同時に実行できるジョブ数の上限（`puzuz gpu parallel`）。各ジョブには
  `CUDA_VISIBLE_DEVICES` と `CUDA_DEVICE_ORDER=PCI_BUS_ID` が設定されます。GPU は NVML で検出します。
- CPU と CUDA プロセスの透過的なチェックポイント/リストア。DataLoader のワーカーを fork する PyTorch の
  ジョブも含みます（[fork ガード](#limitations) を参照）。
- OS のシャットダウンや再起動のときに、実行中の全ジョブを自動でチェックポイントします。
- マルチユーザー：ジョブはその所有者として実行されます。他のユーザーはジョブが存在することは分かりますが、
  そのコマンド、環境変数、ログは見えません。管理者は root と `puzuz-admin` グループです。
- 権限分離：デーモン（HTTP API、スケジューラー、SQLite）は特権のないシステムユーザー `puzuz` として
  動きます。ジョブの起動と CRIU の実行は、小さな root ヘルパーだけが行います。
- ジョブはそれぞれ専用の systemd ユニットで動くため、デーモンやヘルパーを再起動しても（たとえば
  アップグレード中でも）動き続けます。
- `puzuz doctor` がインストール状態を確認し、問題ごとにそれを直すコマンドを表示します。

<a id="quick-start"></a>
## クイックスタート

要件と詳細は [docs/ja/install.md](docs/ja/install.md) を参照してください。

<a id="install-administrator"></a>
### インストール（管理者）

systemd と glibc 2.34 以降を備えた x86_64 または aarch64 の Linux（Ubuntu 22.04 以降、Debian 12 以降、
RHEL 9 以降）では、インストーラーが最新のリリースをダウンロードして検証し、puzuz を /usr/local/bin に
インストールします。

```sh
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

インストーラーは systemd ユニット、`puzuz` ユーザー、`puzuz-admin` グループ、ディレクトリ、デフォルトの
/etc/puzuz/config.toml を用意し、サービスを起動して `puzuz doctor` のレポートを表示します。CRIU と
cuda-checkpoint はインストールしません。足りない場合はそう伝えます。あとで `sudo puzuz update` で更新できます
（実行中のジョブは動き続けます）。削除は `sudo puzuz uninstall` で行い、データは残ります（`--purge` を付けると
データも削除します）。GitHub のトークンは不要です（`GH_TOKEN` が設定されていれば使いますが、API のレート
制限が上がるだけです）。

Debian と Ubuntu（amd64 または arm64）では、代わりにリリースのパッケージをインストールできます。

```sh
sudo apt install ./puzuz_<version>_amd64.deb     # or _arm64.deb
```

同じサービスをインストールして起動し、`criu` を推奨パッケージとします。また puzuz の APT リポジトリ
（GitHub リリースの `apt`）を登録するので、`.deb` を手動でインストールするのは 1 回だけです。以降の
バージョンは `sudo apt update && sudo apt upgrade` で入ります。`sudo apt remove puzuz` はサービスを停止し、
`sudo apt purge puzuz` は /etc/puzuz も削除します。どちらも /var/lib/puzuz と実行中のジョブは残します。

Fedora、RHEL、Rocky、Alma Linux 9 以降（x86_64 または aarch64）では、リリースの .rpm（ファイルまたはその
URL）をインストールします。

```sh
sudo dnf install ./puzuz-<version>-1.x86_64.rpm   # or -1.aarch64.rpm, or the release URL
```

dnf リポジトリはまだありません（予定、issue #23）。更新するには、新しいリリースの .rpm を
`sudo dnf upgrade <its URL>` でインストールします。`sudo dnf remove puzuz` はサービスを停止し、
/var/lib/puzuz と実行中のジョブは残します。`criu` を推奨パッケージとします。Fedora のものは十分に新しい
ですが、Rocky/RHEL 9 には 3.19 しかないため、チェックポイントにはより新しい CRIU が必要です（CRIU が
なくてもジョブは実行できます）。ソースからビルドしたり、Nix flake を使ったりもできます
（[docs/ja/install.md](docs/ja/install.md) を参照）。

NixOS では、flake のモジュール（`github:kaito4681/puzuz/stable`、`nixosModules.default`）をインポートし、
`services.puzuz.enable = true` を設定します（[NixOS](docs/ja/install.md#nixos) を参照）。home-manager の
ユーザーは `homeManagerModules.default` で CLI の補完を追加できます。他のディストリビューション上の Nix
（x86_64 または aarch64）では、flake をビルドし、その結果からサービスをインストールします（バイナリは
/usr/local/bin にコピーされ、Nix のガベージコレクションから保護されます。
[Nix でのインストール](docs/ja/install.md#installing-with-nix) を参照）。

```sh
nix build github:kaito4681/puzuz/stable
sudo ./result/bin/puzuz service install
```

Arch Linux（x86_64、aarch64）では、当面は上記のインストーラーを使ってください。AUR パッケージ `puzuz-bin`
はまだ公開されていません。CRIU（チェックポイントには 4.2 以降）は公式リポジトリにあります。

```sh
sudo pacman -S criu
curl -fsSL https://github.com/kaito4681/puzuz/releases/latest/download/install.sh | sudo bash
```

`sudo puzuz update` で更新し、`sudo puzuz uninstall` で削除します。パッケージが AUR に公開されたら、
代わりに任意の AUR ヘルパーで [`puzuz-bin`](https://aur.archlinux.org/packages/puzuz-bin) を
インストールできます（[docs/ja/install.md](docs/ja/install.md#arch-linux-aur) を参照）。

```sh
# Check everything: criu, cuda-checkpoint, driver, kernel, systemd, services, storage.
puzuz doctor

# Optional: make someone an administrator.
sudo usermod -aG puzuz-admin alice
```

一般ユーザーは何もインストールする必要はありません。

<a id="run-a-gpu-job-any-user"></a>
### GPU ジョブを実行する（全ユーザー）

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
### チェックポイントとリストア

```sh
puzuz checkpoint 0    # save job 0 to disk and stop it; frees the GPU. State: Checkpointed
puzuz restore 0       # queue it again; it resumes once its group and GPUs have a free slot
puzuz restore -i 0    # ...or resume right away, ignoring slot limits
```

リストアされたジョブは、チェックポイントされた瞬間から処理を続けます。ジョブ ID は変わらず、出力は同じログに
追記されます。

一時停止中のジョブもチェックポイントできます（再起動時も同様です）。一時停止したままリストアされ、GPU
スロットも保持したままです。`puzuz start` で再開します。CRIU と cuda-checkpoint は実行中のプロセスを
必要とするため、ダンプの間とリストアの直後に、ジョブは一瞬だけ動きます。チェックポイントの処理中は
`puzuz pause` は拒否されます。

<a id="what-happens-on-reboot"></a>
### 再起動時に何が起きるか

1. OS がシャットダウンするとき、`puzuz-helper.service` はファイルシステムや NVIDIA のサービスより先に
   停止されます。このとき実行中のジョブを 1 つずつチェックポイントします。`--no-checkpoint` のジョブは
   単に終了されます。
2. 起動後、それらのジョブは `Checkpointed` として表示されます。キューで待っていたジョブはそのまま待機し、
   通常どおり開始します。
3. 各所有者（または管理者）が `puzuz restore <id>` か `puzuz restore -a` でジョブを再開します。リストアが
   自動で始まることはありません。

チェックポイントがうまくいかない場合は 2 通りあります。

- **拒否：** ジョブに触れる前にヘルパーのチェックが失敗した場合です（空き容量が足りない、決してリストア
  できないマウントがある、など）。ジョブは動き続けます。`puzuz checkpoint` は理由を示して失敗します。
  ダンプを行う時点になって初めて拒否された場合（たとえば `puzuz shutdown` の最中）、所有者は次に `puzuz`
  コマンドを実行したときに警告を受けます。理由は `puzuz status` と `puzuz log` でジョブの最後のエラーとして
  表示されます。
- **失敗：** ダンプ自体が失敗した場合です。CRIU はジョブを元に戻しますが、ジョブは一度凍結されているため
  停止され（SIGTERM で自分のチェックポイントを保存する機会を与え、`grace_secs` 後に SIGKILL）、理由とともに
  `Checkpoint failed` と表示されます。所有者は次に `puzuz` コマンドを実行したときに警告を受けます。

OS のシャットダウンや再起動の間は、保存できなかったジョブは、ダンプが拒否されたか失敗したかにかかわらず、
すべて systemd によって停止されます。起動後は理由とともに `Checkpoint failed` と表示され、所有者に警告が
出ます。電源断やカーネルのクラッシュでは何も保存されません。

<a id="pueue-compatibility-and-differences"></a>
## pueue との互換性と違い

コマンド名、オプション、表のレイアウトは pueue 4.0 に合わせているので、`pueue` を `puzuz` に置き換えれば、
pueue での習慣やスクリプトの多くはそのまま使えます。puzuz が真似ているのは CLI だけです。pueue の
プロトコルは話さず、pueue のファイルも読みません。違いは次のとおりです。

| 項目 | pueue | puzuz |
| --- | --- | --- |
| デーモン | ユーザーごとに 1 つ、ユーザーが起動（`pueued`） | システム全体で 1 つ、systemd が実行（`puzuz daemon`、ユーザー `puzuz`）。ユーザーがデーモンを起動することはありません。 |
| デーモンの選択 | `-c/--config`、`-p/--profile` | `--socket <path>`（環境変数 `PUZUZ_SOCKET`、デフォルトは `/run/puzuz/puzuz.sock`） |
| `send` | タスクの stdin に入力を送る | 非対応：ジョブの stdin は `/dev/null` です。コマンドを非対話的にしてください。 |
| `shutdown` | 全タスクを kill してデーモンを停止 | 管理者のみ。実行中の全ジョブをチェックポイントしてからデーモンを停止します。チェックポイントが拒否されたジョブは動き続けます。`--no-checkpoint` を付けると代わりに kill します。 |
| `restart` | 終了したタスク | `Checkpointed` のジョブにも使えます。新しいジョブとして最初からやり直し、イメージは残ります。 |
| `reset`、`-a/--all`、`-g/--group` | 全タスク | 自分のジョブ。管理者は全員のジョブに作用します。 |
| `pause/start/kill -a`、`-g` | グループも一時停止/再開 | グループを一時停止/再開するのは管理者だけです。それ以外のユーザーの場合、グループはそのままです。 |
| 用語 | "task" | "job"。`--print-task-id` などのオプション名は互換性のためにそのままです。 |
| `--json` | pueue の内部状態 | puzuz 独自のスキーマ（後述） |
| `edit`、`env`、`restart --edit` | 対応 | 受け付けますが、まだ実装されていません |
| コールバック、エイリアス、`pueue.yml` | 対応 | ありません |

追加のコマンドとオプション：

| コマンド | 用途 |
| --- | --- |
| `add --gpu <LIST>` | ジョブを実行する GPU。例：`0` や `0,1`。指定しなければ CPU のみのジョブになります（`CUDA_VISIBLE_DEVICES` は空）。 |
| `add --no-checkpoint` | このジョブは決してチェックポイントしません。シャットダウン時には終了されます |
| `checkpoint [IDS] [-g GROUP] [-a]` | 実行中のジョブを保存して停止します |
| `restore [IDS] [-g GROUP] [-a] [-i]` | `Checkpointed` のジョブを再開します（`-i`：空きスロットを無視してすぐに） |
| `gpu [-j]`、`gpu parallel <N> [--gpu LIST]` | GPU の一覧を表示するか、GPU ごとのジョブ数の上限を設定します（管理者） |
| `service install [--bin-dir DIR] [--no-start] [--force] / uninstall / status` | systemd サービスを管理します。install は最後のチェックが失敗すると 0 以外で終了します（[docs/ja/install.md](docs/ja/install.md) を参照） |
| `uninstall [--purge] [--yes]` | サービスと、`service install` がコピーしたバイナリを削除します（root が必要） |
| `update [--check] [--version V] [-y]` | 最新（または指定した）リリースに puzuz を更新します。root が必要で、root でなければ確認だけを行います（[docs/ja/install.md](docs/ja/install.md#upgrading) を参照） |
| `doctor` | インストール状態を確認します |
| `daemon` | デーモンをフォアグラウンドで実行します（systemd がこれを行います） |

追加のジョブの状態：`Checkpointing`、`Checkpointed`、`Restore queued`、`Restoring`、`Terminating`
（チェックポイントの失敗後に停止中）。`status` の表には `Owner` と `GPU` の列があり、`Checkpointed` の
ジョブにはイメージのサイズが表示されます。

<a id="json-output"></a>
### JSON 出力

`puzuz status --json` は `{"groups": [...], "jobs": [...]}` を出力します。

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

- タイムスタンプは Unix エポックからのミリ秒です。`owner` は uid です。
- `state` のフィールドはジョブのオブジェクトに展開されます。`state` は `queued`（`enqueued_at`）、
  `stashed`（`enqueue_at` または null）、`running`、`paused`、`checkpointing`、`checkpointed`、
  `restore_queued`（`enqueued_at`）、`restoring`、`terminating`、`done` のいずれかです。
  `terminating` と `done` は `result` を持ち、これは `success`、`failed`（`exit_code`）、
  `failed_to_spawn`（`reason`）、`killed`、`dependency_failed`、`checkpoint_failed`（`reason`）、
  `lost` のいずれかです。
- `checkpoint_size_bytes`、`details`、`details.last_error` は該当しない場合は省略されます。他のユーザーの
  ジョブでは、`label` は null、`details` はなく、`reason` は `"<redacted>"` です。
- `puzuz log --json` は `{"<id>": {"job": <job>, "output": "..."}}` を出力します。`puzuz group --json` は
  `{"groups": [...]}` を出力します。`puzuz gpu --json` は
  `{"gpus": [{"index", "uuid", "name", "memory_total_bytes", "driver_version", "parallel",
  "used", "jobs": [{"id", "owner"}]}]}` を出力し、`"available": false` は消えた GPU を示します。

<a id="multi-user-model-and-authorization"></a>
## マルチユーザーモデルと認可

- デーモンは、ソケットの接続相手についてカーネルが報告する uid（`SO_PEERCRED`）で呼び出し元を識別します。
  クライアントが他人になりすますことはできません。
- 各ジョブは、それを追加したユーザーのものです。そのユーザーの uid、gid、補助グループで、`no_new_privs` を
  設定し、すべてのケーパビリティを落とした状態で実行されます。コマンドは追加したときのディレクトリで、
  `puzuz add` を実行したときの環境変数で実行されます。
- **所有者または管理者のみ：** ジョブの変更（`kill`、`pause`、`start`、`remove`、`checkpoint`、
  `restore`、`restart`、`stash`、`enqueue`、`switch` など）と、そのコマンド、作業ディレクトリ、環境変数、
  ラベル、失敗の理由、ログの閲覧。同様に、`add --after` は呼び出し元が削除できるジョブ（自分のジョブ、
  管理者ならどのジョブでも）だけを受け付けます。ジョブは自分が依存するジョブの削除を妨げるので、他人の
  ジョブに依存できると、その人がジョブを削除したり clean したりできなくなるためです。同じ依存関係を 2 回
  指定しても 1 回として記録されます。
- **全員：** ジョブが存在すること、その所有者、グループ、GPU、状態、優先度、依存関係、時刻の閲覧
  （`status`、`gpu`）。これで誰がどの GPU を使っているかが分かります。
- **管理者のみ：** `group add/remove`、`parallel`、`gpu parallel`、グループ全体の一時停止や再開
  （1 つのグループには `pause/start/kill -g`、全グループには `pause/start/kill -a`）、`shutdown`、他の
  ユーザーのジョブの `reset`。pueue と同様に、`pause` と `kill` は選択したグループを一時停止し、`start` は
  再開します。`pause --wait` はグループだけを一時停止し、実行中のジョブは最後まで実行させます。pueue と
  異なり、`kill -g/-a` はキューに何もなくてもグループを一時停止します。一般ユーザーが
  `pause/start/kill -g` や `-a` を実行した場合は、自分のジョブだけが対象になり、どのグループの一時停止
  状態も変わりません（このとき `pause --wait` は何もしません）。どちらになったかは出力で分かります
  （`All groups are being paused.` と `Your jobs are being paused: 3, 4`）。
- 管理者は root と `puzuz-admin` グループのメンバー（プライマリまたは補助グループ）です。チェックは
  リクエストごとに行われるので、`usermod -aG puzuz-admin <user>` はすぐに反映されます。

<a id="gpu-slots"></a>
## GPU スロット

- `puzuz add --gpu 0,1 ...` は、そのグループに空きスロットがあり、かつ指定した**すべての** GPU に空き
  スロットがあるときにだけジョブを開始します。デーモンは知らない GPU 番号を拒否します。
- デフォルトでは各 GPU で同時に実行できるジョブは 1 つです。管理者は `puzuz gpu parallel 2`（全 GPU）や
  `puzuz gpu parallel 2 --gpu 1` で変更できます。`0` は無制限を意味します。スロットが数えるのはジョブの数で、
  VRAM ではありません。
- GPU 番号は PCI バスの順序に従います。これは `nvidia-smi` や `CUDA_DEVICE_ORDER=PCI_BUS_ID` と同じ順序です。
  ジョブの中では `CUDA_VISIBLE_DEVICES` に要求した GPU が並ぶので、プログラムからは `cuda:0`、`cuda:1`、…
  として見えます。
- グループは pueue と同じように動作します（`default` グループは `parallel 1` で始まります）。両方の上限が
  適用されます。
- GPU はデーモンの起動時に NVML で検出されます（設定の `[[gpus]]` のリストはフォールバックにすぎません）。
  GPU を追加・取り外ししたら `puzuz.service` を再起動してください。`puzuz doctor` はデーモンが検出したものと
  `nvidia-smi` を比較します。

<a id="requirements"></a>
## 要件

- systemd 240 以降の Linux（テスト済み：259）。リリースは x86_64 と aarch64 向けにビルドされますが、実機で
  テストしたのは x86_64 だけです。ビルド済みのバイナリには glibc 2.34 以降が必要です。
- `CONFIG_CHECKPOINT_RESTORE` を有効にしたカーネル。5.5 以降を推奨します（テスト済み：7.0）。
- CRIU 4.2 以降。ディストリビューションのパッケージが十分に新しければそれで動きます（Ubuntu 26.04、Fedora、
  Arch）。古いリリースではソースからビルドしてください（[docs/ja/install.md](docs/ja/install.md#criu) を
  参照）。
- GPU ジョブには、`cuda-checkpoint` が `--action lock/checkpoint/restore/unlock` に対応した NVIDIA
  ドライバー（r570 以降、テスト済み：595）、`cuda-checkpoint` のバイナリ、`nvidia-smi` が必要です。

詳しくは [docs/ja/install.md](docs/ja/install.md) にあります。

<a id="limitations"></a>
## 制限事項

- **ネットワーク接続：** 確立済みの TCP 接続（データセットのサーバー、データベース、実験管理ツールなどへの
  接続）は、ジョブのチェックポイント時に閉じられます（`criu --tcp-close`）。リストア後、プログラムからは
  接続が切れたように見えるので、再接続する必要があります。
- **専用の `/tmp` と `/dev/shm`：** 各ジョブは `/tmp` と `/dev/shm` に自分専用の空の tmpfs を持ちます。
  これはチェックポイントに保存され、他のジョブやシェルとは共有されません。再起動後に残らないマウント
  （`/run` と `/media` 以下のすべて）はジョブの中からは見えません。ファイルをやり取りするには永続的な
  ファイルシステム上のパスを使ってください。
- **マウントは開始時に固定：** ジョブから見えるのは、開始したときにマウントされていたファイルシステム
  （その時点で automount ポイントがトリガーされます）であり、あとからマウントされたものは見えません。
  ジョブをリストアするには、それらのファイルシステムがすべて同じパスに再びマウントされている必要があります。
  そうでなければリストアは拒否され、イメージは残ります。`/scratch` のような tmpfs 上のファイルは再起動後に
  残りません。[ファイルシステムとマウント](docs/ja/install.md#filesystems-and-mounts) を参照してください。
- **クラッシュ耐性なし：** チェックポイントが取られるのは、正常なシャットダウン、`puzuz shutdown`、
  `puzuz checkpoint` のときだけです。電源断、カーネルパニック、ハードリセットでは何も保存されません。定期的な
  チェックポイントはまだありません。学習自体のチェックポイントも引き続き保存してください。
- **fork ガード：** CUDA プロセスが `exec` せずに fork した子プロセス（たとえば `fork` 開始方式の PyTorch
  DataLoader ワーカー）は、CRIU が保存できない NVIDIA デバイスのマッピングを引き継ぎます。ヘルパーは
  すべてのジョブに小さなライブラリをプリロード（`LD_PRELOAD`）し、子プロセスでそれらのマッピングを削除します。
  静的リンクされたプログラムや、生の `clone(2)` や `_Fork()` で作られたプロセスでは機能しません。ガードが
  あると、そのように fork された子プロセスでは NVML を初期化できません。PyTorch は警告を表示してそのまま
  続行します。`fork_guard = false` で無効にできます。
- **同じ GPU モデルとドライバー：** ジョブをリストアできるのは、チェックポイントしたときと同じモデルの GPU で、
  同じドライバーバージョン、同じ GPU 番号の場合だけです。そうでなければリストアは拒否され、イメージは残ります。
  NVIDIA ドライバーのアップグレードは、チェックポイント済みのジョブをすべてリストア（または削除）してから
  行ってください。
- **同じファイル：** CRIU はジョブのファイルをパスで開き直し、実行ファイルとライブラリを検査します。リストア
  するまで、ジョブの virtualenv、バイナリ、作業ディレクトリ、ログを変更したり削除したりしないでください。
  ジョブのファイルへの経路上にあるディレクトリをシンボリックリンクに置き換えないでください。また、所属していた
  グループからは抜けないでください。リストアは、ジョブが開いていたすべてのファイルを、同じディレクトリを
  通って同じ方法で開けることを確認し、できなければ拒否されます（[リストアとパス](docs/ja/architecture.md#restores-and-paths)
  を参照）。
- **Unix ソケット：** 自分の `/tmp` と `/dev/shm` の外（たとえばホームディレクトリ）のパス、または
  `ctl.sock` のような相対名にバインドした Unix ソケットを持つジョブはチェックポイントできません。
  チェックポイントは拒否され、ジョブは動き続けます。ソケットは `/tmp` 以下の絶対パスにバインドするか、
  抽象名を使ってください。
- **`SO_REUSEPORT`：** `SO_REUSEPORT` を設定した TCP または UDP のソケットを持つジョブもチェックポイント
  できません（CRIU はそのソケットを root のものとしてリストアするので、root のサービスとポートを共有できて
  しまいます）。デフォルトでこれを設定するサーバーもあります。無効にするか、`--no-checkpoint` を使ってください。
- **Python と PyTorch：**
  - Python 3.14 以降は Linux でのデフォルトの開始方式が `forkserver` なので、DataLoader ワーカーは fork
    されず、fork ガードにも依存しません。Python 3.12 と 3.13（または明示的な
    `multiprocessing_context="fork"`）は fork ガードに依存します。
  - プロセス間での CUDA テンソルの共有（`torch.multiprocessing` の CUDA IPC）は、現在のドライバーでは
    チェックポイントできません。NCCL を使うマルチ GPU のジョブはテストしていません。
  - cuda-checkpoint は保存の前に投入済みの GPU の処理が終わるのを待つので、非常に長いカーネルは
    チェックポイントを遅らせます。
  - チェックポイントにはジョブごとにおよそ RSS + VRAM にジョブが `/tmp` と `/dev/shm` に置いている分を
    加えたディスク容量が必要で、1 ジョブずつ書き込まれます。
    テストでは、7 GB の VRAM を使うジョブのダンプに約 10〜20 秒かかりました。
- **`/` を覆うマウント：** ルートディレクトリが別のマウントで覆われているジョブはチェックポイントできません。
  puzuz はダンプを拒否し、理由を伝えます。ジョブは動き続けます。
- チェックポイントのイメージは、そのジョブが存在する間は自動で削除されることはありません。削除されるのは、
  ジョブのリストアが成功したとき、ジョブが削除されたとき（`remove`、`clean`、`reset`）、またはダンプが
  失敗したときです。`checkpoint_size_limit_bytes` を超えると puzuz は警告しますが、拒否はしません。
- まだないもの：`edit`、`env`、定期的なチェックポイント、ジョブを動かしたままのチェックポイント、完全な
  チェックポイントなしでの GPU の解放（"GPU suspend"）、Web UI や TCP API、VRAM で数えるスロット。

<a id="project-status"></a>
## プロジェクトの状況

プレリリース（バージョン 0.1.0）。これまでにテストしたもの：

- すべてのクレートのユニットテストと統合テスト（`scripts/test-in-docker.sh`）。
- PID 1 の systemd 259、CRIU 4.2、`puzuz service install` でインストールしたバイナリを使った、特権 Docker
  コンテナ内でのフルスタックのエンドツーエンドテスト（`scripts/e2e-test.sh`）。テストは、基本的なキュー操作、
  3 ユーザーでの認可、グループと GPU の上限、チェックポイント/リストア、デーモンとヘルパーの再起動、一度に
  大量に追加したジョブ、`puzuz shutdown`、そして擬似的な再起動をカバーしています。擬似的な再起動では、
  コンテナを systemd でシャットダウンし、新しいコンテナでジョブをリストアします。別の ext4 ファイルシステム、
  バインドマウント、`x-systemd.automount`、overlayfs、tmpfs、sshfs 上で作業するジョブで、それらが新しい
  マウントとして戻ってくる場合も含みます。
- Docker 内の本物の GPU（ドライバー 595、manual モードの CRIU 4.2.1、Python 3.12 上の PyTorch 2.8）で：
  CUDA のカウンターと、fork された DataLoader ワーカーを使う PyTorch の学習のチェックポイント/リストア
  （結果は中断しなかった実行と同一）、およびパッケージ化されサンドボックス化された `puzuz.service` 内での
  デーモンによる NVML の GPU 検出。

まだテストしていないもの：

- 実機での実際の OS の再起動：ドライバーの再読み込みと、ファイルシステムや `nvidia-persistenced` に対する
  停止順序。
- 実際の GitHub リリースからの `install.sh` によるインストール（偽のリリースサーバーに対しては、
  Ubuntu 22.04/24.04/26.04 と Debian 12 のコンテナで `scripts/dist-test.sh` によりテスト済み）。
- 動作中の systemd の下での `.deb`（インストール、再インストール、削除、purge は systemd のない素の
  Debian 12 と Ubuntu のコンテナで `scripts/deb-test.sh` によりテスト済み。APT リポジトリのローカルコピーを
  通したアップグレードも `scripts/apt-repo-test.sh` でテスト済み）、`.rpm` についても同様（インストール、
  アップグレード、削除は素の Rocky Linux 9 と Fedora のコンテナで `scripts/rpm-test.sh` によりテスト済み）、
  実際の `apt` リリースにある APT リポジトリ、実機での arm64。
- カーネルの NFS と autofs の間接マップ（FUSE のネットワークファイルシステムと `x-systemd.automount` は
  テスト済み）。
- マルチ GPU と NCCL のジョブ、テストに使ったもの以外の GPU モデル。

お使いのハードウェアで何が動いて何が動かないか、ぜひ報告してください。

<a id="documentation"></a>
## ドキュメント

- [docs/ja/install.md](docs/ja/install.md)：要件、ビルド、インストール、設定、アップグレード、アンインストール、
  トラブルシューティング
- [docs/ja/architecture.md](docs/ja/architecture.md)：コンポーネント、ジョブの流れ、シャットダウンの順序、
  状態の保存、クレートの構成

<a id="contributing"></a>
## コントリビューション

ワークスペースの構成と規約については [AGENTS.md](AGENTS.md) を読んでください。コード、コメント、
ドキュメント、コミットメッセージは英語で書きます。テストは Docker 内で実行します。

```sh
scripts/test-in-docker.sh                                   # cargo test --workspace
scripts/test-in-docker.sh clippy --workspace --all-targets
scripts/test-in-docker.sh fmt --all
scripts/e2e-test.sh               # full stack under systemd in a privileged container
scripts/service-install-test.sh   # service install / status / uninstall and doctor
scripts/helper-systemd-test.sh    # the helper alone under systemd
scripts/dist-test.sh              # install.sh against a fake GitHub API (PUZUZ_DIST_BASE=debian:12 ...)
nix build -L .#checks.x86_64-linux.nixos         # NixOS module in a VM (needs KVM)
nix build -L .#checks.x86_64-linux.home-manager  # home-manager module evaluation
nix build -L .#checks.x86_64-linux.helper-tests  # all puzuz-helper tests in a VM (needs KVM)
```

`scripts/e2e-test.sh` は冒頭で環境変数を説明しています。たとえば `PUZUZ_E2E_ONLY` や、GPU のシナリオ用の
`PUZUZ_E2E_GPU=1` です。`PUZUZ_HELPER_GPU=1 scripts/helper-systemd-test.sh`（CUDA のベースイメージが必要。
スクリプトを参照）は、どちらの `PUZUZ_HELPER_CUDA_MODE` でも、GPU のチェックポイントを期限切れに追い込み、
ダンプの途中で `criu` / `cuda-checkpoint` を kill します。`scripts/measure-checkpoint.sh` はチェックポイントの
各ステップにかかる時間を測定します。どのスクリプトもホスト上で特権の必要な処理は実行しません。

<a id="license"></a>
## ライセンス

[MIT](LICENSE)
