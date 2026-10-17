<a id="using-gpueue"></a>
# gpueue の使い方

[English](../usage.md) | 日本語 | [简体中文](../zh-CN/usage.md)

インストール後に gpueue でジョブを実行するためのリファレンスです。概要とクイックスタートは
[README](../../README.ja.md) に、インストール、設定、運用は [install.md](install.md) にあります。

- [pueue との互換性と違い](#pueue-compatibility-and-differences)
- [JSON 出力](#json-output)
- [チェックポイントとリストア](#checkpoint-and-restore)
  - [再起動時に何が起きるか](#what-happens-on-reboot)
- [マルチユーザーモデルと認可](#multi-user-model-and-authorization)
- [GPU スロット](#gpu-slots)
- [ジョブの優先度](#job-priority)
- [制限事項](#limitations)
- [テストの範囲](#test-coverage)

<a id="pueue-compatibility-and-differences"></a>
## pueue との互換性と違い

コマンド名、オプション、表のレイアウトは pueue 4.0 に合わせているので、`pueue` を `gpueue` に置き換えれば、
pueue での習慣やスクリプトの多くはそのまま使えます。gpueue が真似ているのは CLI だけです。pueue の
プロトコルは話さず、pueue のファイルも読みません。違いは次のとおりです。

| 項目 | pueue | gpueue |
| --- | --- | --- |
| デーモン | ユーザーごとに 1 つ、ユーザーが起動（`pueued`） | システム全体で 1 つ、systemd が実行（`gpueue daemon`、ユーザー `gpueue`）。ユーザーがデーモンを起動することはありません。 |
| デーモンの選択 | `-c/--config`、`-p/--profile` | `--socket <path>`（環境変数 `GPUEUE_SOCKET`、デフォルトは `/run/gpueue/gpueue.sock`） |
| `send` | タスクの stdin に入力を送る | 非対応：ジョブの stdin は `/dev/null` です。コマンドを非対話的にしてください。 |
| `shutdown` | 全タスクを kill してデーモンを停止 | 管理者のみ。実行中の全ジョブをチェックポイントしてからデーモンを停止します。チェックポイントが拒否されたジョブは動き続けます。`--no-checkpoint` を付けると代わりに kill します。 |
| `restart` | 終了したタスク | `Checkpointed` のジョブにも使えます。新しいジョブとして最初からやり直し、イメージは残ります。 |
| `start <id>`、`add -i`、`restart -i`（と `restore -i`） | すべての上限を無視してすぐにタスクを開始 | すべての上限を無視してすぐに開始するのは管理者だけです。それ以外のユーザーのジョブは自分のジョブの先頭にキューされ（`gpueue top` と同じ）、ほかのキュー中のジョブと同じく順番を待ちます（[ジョブの優先度](#job-priority)を参照） |
| `reset`、`-a/--all`、`-g/--group` | 全タスク | 自分のジョブ。管理者は全員のジョブに作用します。 |
| `pause/start/kill -a`、`-g` | グループも一時停止/再開 | グループを一時停止/再開するのは管理者だけです。それ以外のユーザーの場合、グループはそのままです。 |
| 用語 | "task" | "job"。`--print-task-id` などのオプション名は互換性のためにそのままです。 |
| `--json` | pueue の内部状態 | gpueue 独自のスキーマ（後述） |
| `edit`、`env`、`restart --edit` | 対応 | 受け付けますが、まだ実装されていません |
| `add -o/--priority`、`switch` | デーモンの全タスクを並べ替える | 自分のジョブだけを並べ替えます。どのユーザーのジョブを先に開始するかは、フェアシェアを含む Slurm 式のマルチファクター優先度で決まります（[ジョブの優先度](#job-priority) を参照）。`priority_type = "basic"` で pueue の順序に戻りますが、`-o` が並べ替えるのは引き続き自分のジョブだけです。 |
| コールバック、エイリアス、`pueue.yml` | 対応 | ありません |

追加のコマンドとオプション：

| コマンド | 用途 |
| --- | --- |
| `add --gpu <LIST>` | ジョブを実行する GPU。例：`0` や `0,1`。指定しなければ CPU のみのジョブになります（`CUDA_VISIBLE_DEVICES` は空）。 |
| `add --no-checkpoint` | このジョブは決してチェックポイントしません。シャットダウン時には終了されます |
| `checkpoint [IDS] [-g GROUP] [-a]` | 実行中のジョブを保存して停止します |
| `restore [IDS] [-g GROUP] [-a] [-i]` | `Checkpointed` のジョブを再開します（`-i`：`start` と同じく、管理者ならすぐ、それ以外は自分のジョブの先頭に） |
| `gpu [-j]`、`gpu parallel <N> [--gpu LIST]` | GPU の一覧を表示するか、GPU ごとのジョブ数の上限を設定します（管理者） |
| `add --nice <N>`、`nice <IDS>... <N>` | ジョブの優先度を `N` だけ下げます（Slurm の nice。負の値は管理者のみ） |
| `top <IDS>...` | ジョブを自分のキューの先頭に移します（Slurm の `scontrol top`） |
| `prio [IDS] [-j]` | 待機中のジョブをスケジュール順に、優先度の要素とともに表示します（Slurm の `sprio` に相当） |
| `share [-j]`、`share set <USER> <N>` | ユーザーごとのフェアシェアの状態を表示します（Slurm の `sshare` に相当）。ユーザーのシェアを設定します（管理者） |
| `site <IDS>... <N>`、`group priority <GROUP> <N>` | ジョブのサイトファクター、グループの優先度を設定します（管理者） |
| `service install [--bin-dir DIR] [--no-start] [--force] / uninstall / status` | systemd サービスを管理します。install は最後のチェックが失敗すると 0 以外で終了します（[install.md](install.md) を参照） |
| `uninstall [--purge] [--yes]` | サービスと、`service install` がコピーしたバイナリを削除します（root が必要） |
| `update [--check] [--version V] [-y]` | 最新（または指定した）リリースに gpueue を更新します。root が必要で、root でなければ確認だけを行います（[install.md](install.md#upgrading) を参照） |
| `doctor` | インストール状態を確認します |
| `daemon` | デーモンをフォアグラウンドで実行します（systemd がこれを行います） |

追加のジョブの状態：`Checkpointing`、`Checkpointed`、`Restore queued`、`Restoring`、`Terminating`
（チェックポイントの失敗後に停止中）。`status` の表には `Owner` と `GPU` の列があり、`Checkpointed` の
ジョブにはイメージのサイズが表示されます。

<a id="json-output"></a>
## JSON 出力

`gpueue status --json` は `{"groups": [...], "jobs": [...]}` を出力します。

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
- `checkpoint_size_bytes`、`details`、`details.last_error`、ジョブの `nice`、グループの `priority` は
  該当しない場合（または 0 の場合）は省略されます。他のユーザーのジョブでは、`label` は null、`details` はなく、`reason` は `"<redacted>"` です。
- `waiting` は、管理者以外がすぐに実行しようとしたものの GPU スロットか一時停止中のグループを待っている
  キュー中のジョブにだけ付きます：`{"reason": "gpu_slots", "gpus": [0]}`、`{"reason": "group_paused"}`、
  `{"reason": "unavailable"}` のいずれかです。
- `gpueue log --json` は `{"<id>": {"job": <job>, "output": "..."}}` を出力します。`gpueue group --json` は
  `{"groups": [...]}` を出力します。`gpueue gpu --json` は
  `{"gpus": [{"index", "uuid", "name", "memory_total_bytes", "driver_version", "parallel",
  "used", "jobs": [{"id", "owner"}]}]}` を出力し、`"available": false` は消えた GPU を示します。
- `gpueue prio --json` は `{"priority_type", "weights": {"age", "fairshare", "job_size",
  "partition", "tres"}, "jobs": [{"id", "owner", "group", "priority", "factors": {"age",
  "fairshare", "job_size", "partition", "tres_cpu", "tres_gpu"}, "weighted": {"age", "fairshare",
  "job_size", "partition", "tres"}, "site", "nice"}]}` を出力し、ジョブはスケジュール順に並びます。
  `gpueue share --json` は `{"users": [{"uid", "shares", "norm_shares", "raw_usage",
  "effective_usage", "fairshare", "active"}], "decay_half_life_secs", "calc_period_secs",
  "usage_reset_period", "billing_weights", "usage_calc_at"}` を出力します。

<a id="checkpoint-and-restore"></a>
## チェックポイントとリストア

`gpueue checkpoint <id>` は実行中（または一時停止中）のジョブをディスクに保存して停止し、GPU を解放します。
ジョブは `Checkpointed` になります。`gpueue restore <id>` はそれを再びキューに入れ、グループと GPU に空き
スロットができると再開します（`restore -i`：自分のキュー中のジョブの先頭に、管理者ならすぐに。
[ジョブの優先度](#job-priority)を参照）。

リストアされたジョブは、チェックポイントされた瞬間から処理を続けます。ジョブ ID は変わらず、出力は同じログに
追記されます。

一時停止中のジョブもチェックポイントできます（再起動時も同様です）。一時停止したままリストアされ、GPU
スロットも保持したままです。`gpueue start` で再開します。CRIU と cuda-checkpoint は実行中のプロセスを
必要とするため、ダンプの間はジョブが一瞬だけ動きます。リストアは停止したままのジョブを戻すので、
`gpueue start` までジョブは（一瞬たりとも）何も実行しません。チェックポイントの処理中は `gpueue pause` は
拒否されます。

<a id="what-happens-on-reboot"></a>
### 再起動時に何が起きるか

1. OS がシャットダウンするとき、`gpueue-helper.service` はファイルシステムや NVIDIA のサービスより先に
   停止されます。このとき実行中のジョブを 1 つずつチェックポイントします。`--no-checkpoint` のジョブは
   単に終了されます。
2. 起動後、それらのジョブは `Checkpointed` として表示されます。キューで待っていたジョブはそのまま待機し、
   通常どおり開始します。
3. 各所有者（または管理者）が `gpueue restore <id>` か `gpueue restore -a` でジョブを再開します。リストアが
   自動で始まることはありません。

チェックポイントがうまくいかない場合は 2 通りあります。

- **拒否：** ジョブに触れる前にヘルパーのチェックが失敗した場合です（空き容量が足りない、決してリストア
  できないマウントがある、など）。ジョブは動き続けます。`gpueue checkpoint` は理由を示して失敗します。
  ダンプを行う時点になって初めて拒否された場合（たとえば `gpueue shutdown` の最中）、所有者は次に `gpueue`
  コマンドを実行したときに警告を受けます。理由は `gpueue status` と `gpueue log` でジョブの最後のエラーとして
  表示されます。
- **失敗：** ダンプ自体が失敗した場合です。CRIU はジョブを元に戻しますが、ジョブは一度凍結されているため
  停止され（SIGTERM で自分のチェックポイントを保存する機会を与え、`grace_secs` 後に SIGKILL）、理由とともに
  `Checkpoint failed` と表示されます。所有者は次に `gpueue` コマンドを実行したときに警告を受けます。

OS のシャットダウンや再起動の間は、保存できなかったジョブは、ダンプが拒否されたか失敗したかにかかわらず、
すべて systemd によって停止されます。起動後は理由とともに `Checkpoint failed` と表示され、所有者に警告が
出ます。電源断やカーネルのクラッシュでは何も保存されません。

<a id="multi-user-model-and-authorization"></a>
## マルチユーザーモデルと認可

- デーモンは、ソケットの接続相手についてカーネルが報告する uid（`SO_PEERCRED`）で呼び出し元を識別します。
  クライアントが他人になりすますことはできません。
- 各ジョブは、それを追加したユーザーのものです。そのユーザーの uid、gid、補助グループで、`no_new_privs` を
  設定し、すべてのケーパビリティを落とした状態で実行されます。コマンドは追加したときのディレクトリで、
  `gpueue add` を実行したときの環境変数で実行されます。
- root ヘルパーは、システムアカウント（uid が `job_min_uid` 未満。既定は `/etc/login.defs` の `UID_MIN`）、
  `nobody`、systemd のダイナミックユーザーの uid（61184-65519）、`job_max_uid` が設定されているときはそれを
  超える uid、`job_allowed_users` が設定されているときにそこに含まれないユーザー、プライマリグループが root 相当の
  グループ（`job_denied_groups`。既定は `docker`、`lxd`、`incus-admin`、`libvirt`、`libvirtd`、`disk`、`sudo`、
  `wheel`、`admin`）であるユーザーのジョブを拒否します。これらのグループはすべてのジョブの補助グループから
  外されるので、所有者が Docker や sudo を使えてもジョブからは使えません（外したグループはヘルパーのジャーナルに記録されます）。
  拒否されたジョブは `gpueue refuses to run jobs as <user>: <reason>` という理由で起動に失敗します。
  [ジョブを実行できるユーザー](install.md#who-jobs-may-run-as)を参照してください。
- 所有者が root になれる場合でも、ジョブは root になることも root として操作することもできません。`no_new_privs`
  のもとでは setuid プログラム（`sudo`、`su`、`pkexec`）は権限を得られず、ジョブのユニットからはシステムの D-Bus
  に接続できないので、polkit のルールが所有者のグループに何を許していても `systemctl`、`run0`、`machinectl`
  などの polkit で認可される操作はできません（`job_system_dbus = true` で解除できます）。`/run/systemd` の下の、
  polkit で認可する systemd の Varlink サービスにも接続できません（`job_systemd_varlink`）。所有者のグループを
  ユーザーデータベースで確かめるデーモンのソケット（polkit で認可する `/run/libvirt` の libvirt のもの、`/run/cups` の
  CUPS のもの）にも接続できません（`job_hidden_socket_dirs`）。root 相当のグループは
  外され、開けるデバイスノードは割り当てた GPU のものだけです。リストアの後も同じです。
  [ジョブにできること・できないこと](install.md#what-a-job-can-and-cannot-do)を参照してください。
- **所有者または管理者のみ：** ジョブの変更（`kill`、`pause`、`start`、`remove`、`checkpoint`、
  `restore`、`restart`、`stash`、`enqueue`、`switch` など）と、そのコマンド、作業ディレクトリ、環境変数、
  ラベル、失敗の理由、ログの閲覧。同様に、`add --after` は呼び出し元が削除できるジョブ（自分のジョブ、
  管理者ならどのジョブでも）だけを受け付けます。ジョブは自分が依存するジョブの削除を妨げるので、他人の
  ジョブに依存できると、その人がジョブを削除したり clean したりできなくなるためです。同じ依存関係を 2 回
  指定しても 1 回として記録されます。
- **全員：** ジョブが存在すること、その所有者、グループ、GPU、状態、優先度、依存関係、時刻の閲覧
  （`status`、`gpu`）、待機中のジョブの優先度の要素（`prio`）、全員のフェアシェアの使用量（`share`）。
  これで誰がどの GPU を使っているかが分かります。
- **所有者：** `nice` を上げること（0 未満にはできません）と、`top`（`enable_user_top = false` でない場合）。
- **管理者のみ：** `share set`、`group priority`、`site`、負の値の `nice`、他のユーザーのジョブの `top`、
  `group add/remove`、`parallel`、`gpu parallel`、グループ全体の一時停止や再開
  （1 つのグループには `pause/start/kill -g`、全グループには `pause/start/kill -a`）、`shutdown`、他の
  ユーザーのジョブの `reset`、[孤立ジョブ](architecture.md#orphans)（デーモンが把握していないのにヘルパーが
  実行しているプロセス。`status` が管理者に表示します）の `kill <id>`。pueue と同様に、`pause` と `kill` は選択したグループを一時停止し、`start` は
  再開します。`pause --wait` はグループだけを一時停止し、実行中のジョブは最後まで実行させます。pueue と
  異なり、`kill -g/-a` はキューに何もなくてもグループを一時停止します。一般ユーザーが
  `pause/start/kill -g` や `-a` を実行した場合は、自分のジョブだけが対象になり、どのグループの一時停止
  状態も変わりません（このとき `pause --wait` は何もしません）。どちらになったかは出力で分かります
  （`All groups are being paused.` と `Your jobs are being paused: 3, 4`）。
- **ジョブをすぐに実行できるのは管理者だけです**（`start <id>`、`add -i`、`restart -i`、`restore -i`）。
  管理者のものはすべての上限を無視します。それ以外のユーザーのジョブは自分のジョブの先頭にキューされます
  （[ジョブの優先度](#job-priority)を参照）。
- 管理者は root と `gpueue-admin` グループのメンバー（プライマリまたは補助グループ）です。チェックは
  リクエストごとに行われるので、`usermod -aG gpueue-admin <user>` はすぐに反映されます。

<a id="gpu-slots"></a>
## GPU スロット

- `gpueue add --gpu 0,1 ...` は、そのグループに空きスロットがあり、かつ指定した**すべての** GPU に空き
  スロットがあるときにだけジョブを開始します。デーモンは知らない GPU 番号を拒否します。
- デフォルトでは各 GPU で同時に実行できるジョブは 1 つです。管理者は `gpueue gpu parallel 2`（全 GPU）や
  `gpueue gpu parallel 2 --gpu 1` で変更できます。`0` は無制限を意味します。スロットが数えるのはジョブの数で、
  VRAM ではありません。
- GPU 番号は PCI バスの順序に従います。これは `nvidia-smi` や `CUDA_DEVICE_ORDER=PCI_BUS_ID` と同じ順序です。
  ジョブの中では `CUDA_VISIBLE_DEVICES` に要求した GPU が並ぶので、プログラムからは `cuda:0`、`cuda:1`、…
  として見えます。
- **GPU の分離：** ジョブが開けるのは、割り当てられた GPU のデバイスノードだけです。すべてのジョブの
  ユニットには systemd の `DevicePolicy=closed` が設定され、`DeviceAllow=` で割り当てた GPU の
  `/dev/nvidia<N>` と共通の NVIDIA ノードだけが許可されます。CPU だけのジョブは NVIDIA デバイスをまったく
  開けません。`CUDA_VISIBLE_DEVICES` を書き換えたり、他の GPU のノードを開いたりしても `EPERM` で失敗し、
  ジョブの中の `nvidia-smi` には自分の GPU だけが表示されます（CPU だけのジョブでは
  `Failed to initialize NVML: Unknown Error` で失敗します）。管理者は `gpu_device_isolation = false` で
  無効にできます。MIG を使うときなどに必要です（[GPU の分離](install.md#gpu-isolation) を参照）。
- グループは pueue と同じように動作します（`default` グループは `parallel 1` で始まります）。両方の上限が
  適用されます。
- GPU スロット、グループの `parallel` の上限、一時停止中のグループは、一般ユーザーが起動するすべてのジョブに
  適用されます。`start <id>`、`add -i`、`restart -i`、`restore -i` でも同じです（[ジョブの優先度](#job-priority)を
  参照）。これらを無視するのは管理者の即時実行だけです。
- GPU はデーモンの起動時に NVML で検出されます（設定の `[[gpus]]` のリストはフォールバックにすぎません）。
  GPU を追加・取り外ししたら `gpueue.service` を再起動してください。`gpueue doctor` はデーモンが検出したものと
  `nvidia-smi` を比較します。

<a id="job-priority"></a>
## ジョブの優先度

複数のジョブが同じスロットを待っているときは、収まるジョブのうち優先度が最も高いものが先に開始します。
優先度は Slurm の multifactor プラグインに従います。デフォルトでは、フェアシェアの要素（最近マシンをあまり
使っていないユーザーが先。使用量は半減期 7 日で減衰します）と待ち時間の要素（待っていた時間）の合計から、
ジョブの nice 値を引いたものです。ジョブのサイズ、グループ（パーティション）、GPU ごと（TRES）の要素を
重みに加えることもできます。自分で付けた `-o` の優先度、`switch`、`gpueue top` が並べ替えるのは自分の
ジョブだけです。管理者以外が pueue の即時実行（キュー中または stash されたジョブの `start <id>`、`add -i`、
`restart -i`、`restore -i`）を使った場合も同じで、ジョブは `gpueue top` とまったく同じように自分のジョブの
先頭にキューされ、その後はグループの `parallel` の上限、GPU スロット、一時停止中のグループ、依存関係の
もとで他のユーザーのジョブと順番を競います。コマンドは、開始ではなくキューされたことを表示します。
`enable_user_top = false`（または `priority_type = "basic"`）のときは、単にキューされます。一時停止中の
ジョブの `start` はこれまでどおり再開です。管理者の即時実行はすべての上限を無視してすぐに開始します。`gpueue prio` はジョブがその位置にある理由を、`gpueue share` は全員の使用量を表示します。
計算式と設定は [優先度とフェアシェア](install.md#priority-and-fair-share) を参照してください。

<a id="limitations"></a>
## 制限事項

- **ネットワーク接続：** 確立済みの TCP 接続（データセットのサーバー、データベース、実験管理ツールなどへの
  接続）は、ジョブのチェックポイント時に閉じられます（`criu --tcp-close`）。リストア後、プログラムからは
  接続が切れたように見えるので、再接続する必要があります。
- **専用の `/tmp` と `/dev/shm`：** 各ジョブは `/tmp` と `/dev/shm` に自分専用の空の tmpfs を持ちます。
  これはチェックポイントに保存され、他のジョブやシェルとは共有されません。再起動後に残らないマウント
  （`/run` と `/media` 以下のすべて）とほかのコンテナのマウント（`/var/lib/docker/` 以下など）はジョブの中からは見えません。ファイルをやり取りするには永続的な
  ファイルシステム上のパスを使ってください。
- **マウントは開始時に固定：** ジョブから見えるのは、開始したときにマウントされていたファイルシステム
  （その時点で automount ポイントがトリガーされます）であり、あとからマウントされたものは見えません。
  ジョブをリストアするには、それらのファイルシステムがすべて同じパスに再びマウントされている必要があります。
  そうでなければリストアは拒否され、イメージは残ります。`/scratch` のような tmpfs 上のファイルは再起動後に
  残りません。[ファイルシステムとマウント](install.md#filesystems-and-mounts) を参照してください。
- **クラッシュ耐性なし：** チェックポイントが取られるのは、正常なシャットダウン、`gpueue shutdown`、
  `gpueue checkpoint` のときだけです。電源断、カーネルパニック、ハードリセットでは何も保存されません。定期的な
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
  通って同じ方法で開けることを確認し、できなければ拒否されます（[リストアとパス](architecture.md#restores-and-paths)
  を参照）。
- **Unix ソケット：** 自分の `/tmp` と `/dev/shm` の外（たとえばホームディレクトリ）のパス、または
  `ctl.sock` のような相対名にバインドした Unix ソケットを持つジョブはチェックポイントできません。
  チェックポイントは拒否され、ジョブは動き続けます。ソケットは `/tmp` 以下の絶対パス（途中に `..` や
  シンボリックリンクを含まないもの）にバインドするか、listen しないソケットには抽象名を使ってください。
  抽象名で *listen* しているソケットはチェックポイントできません（CRIU がそれを root として設定するため、
  クライアントからは root が listen しているように見えてしまいます）。ただし、libcuda がすべての CUDA
  プロセスで listen するソケット（`@cuda-uvmfd-<PID 名前空間の inode>-<PID>@`）は例外です。
- **inotify、fanotify、転送中のディスクリプタ：** inotify または fanotify のディスクリプタ（ファイル監視）を
  持つジョブ、またはチェックポイントの時点で Unix ソケット越しにディスクリプタを渡している（`SCM_RIGHTS`）
  ジョブはチェックポイントできません（CRIU がそれらを root として再作成してしまうため）。
- **リッスンポート：** TCP または UDP のソケットがポートにバインドされているジョブ（Jupyter、TensorBoard）
  もチェックポイントできますが、リストアされるのはホスト上の他のソケットがそのポートを使っていないときだけです。
  使われている場合はリストアが拒否され、ポートが空いてから再試行できます。
- **`SO_REUSEPORT`：** `SO_REUSEPORT` を設定した TCP または UDP のソケットを持つジョブもチェックポイント
  できません（CRIU はそのソケットを root のものとしてリストアするので、root のサービスとポートを共有できて
  しまいます）。デフォルトでこれを設定するサーバーもあります。無効にするか、`--no-checkpoint` を使ってください。
- **削除済みファイル：** ジョブが開いたまま削除したファイル（一時ファイル、multiprocessing のセマフォ）は
  イメージに保存されます。別のハードリンクが残っている削除済みファイル、開いたまま NFS 上で削除されたファイル
  （`.nfs<16進数>`）、setuid / setgid ビットかファイルケーパビリティを持つ削除済みのプログラムを持つジョブは
  チェックポイントできません。チェックポイントは拒否され、ジョブは動き続けます。実行中にパッケージの更新で
  置き換えられたライブラリは問題ありません。
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
  gpueue はダンプを拒否し、理由を伝えます。ジョブは動き続けます。
- チェックポイントのイメージは、そのジョブが存在する間は自動で削除されることはありません。削除されるのは、
  ジョブのリストアが成功したとき、ジョブが削除されたとき（`remove`、`clean`、`reset`）、またはダンプが
  失敗したときです。`checkpoint_size_limit_bytes` を超えると gpueue は警告しますが、拒否はしません。
- まだないもの：`edit`、`env`、定期的なチェックポイント、ジョブを動かしたままのチェックポイント、完全な
  チェックポイントなしでの GPU の解放（"GPU suspend"）、Web UI や TCP API、VRAM で数えるスロット。

<a id="test-coverage"></a>
## テストの範囲

これまでにテストしたもの：

- すべてのクレートのユニットテストと統合テスト（`scripts/test-in-docker.sh`）。
- PID 1 の systemd 259、CRIU 4.2、`gpueue service install` でインストールしたバイナリを使った、特権 Docker
  コンテナ内でのフルスタックのエンドツーエンドテスト（`scripts/e2e-test.sh`）。テストは、基本的なキュー操作、
  3 ユーザーでの認可、グループと GPU の上限、チェックポイント/リストア、デーモンとヘルパーの再起動、一度に
  大量に追加したジョブ、`gpueue shutdown`、そして擬似的な再起動をカバーしています。擬似的な再起動では、
  コンテナを systemd でシャットダウンし、新しいコンテナでジョブをリストアします。別の ext4 ファイルシステム、
  バインドマウント、`x-systemd.automount`、overlayfs、tmpfs、sshfs 上で作業するジョブで、それらが新しい
  マウントとして戻ってくる場合も含みます。
- Docker 内の本物の GPU（ドライバー 595、manual モードの CRIU 4.2.1、Python 3.12 上の PyTorch 2.8）で：
  CUDA のカウンターと、fork された DataLoader ワーカーを使う PyTorch の学習のチェックポイント/リストア
  （結果は中断しなかった実行と同一）、およびパッケージ化されサンドボックス化された `gpueue.service` 内での
  デーモンによる NVML の GPU 検出。
- QEMU/KVM 仮想マシン上の実際の OS（`scripts/vm-reboot-test.sh`）：各ディストリビューションのクラウド
  イメージから起動し、ユーザーと同じ方法で gpueue をインストールします。`.deb` は
  `apt install ./gpueue_<version>_amd64.deb`（Fedora では `.rpm` を `dnf` で）、または
  `gpueue service install` を使い、`/var/lib/gpueue` は専用の ext4 ディスクに置きます。Ubuntu 26.04
  （systemd 259、CRIU 4.2）では、`systemctl reboot`、`systemctl poweroff` の後の新規起動、そして 1 GiB の
  ジョブの `gpueue checkpoint` がまだ実行中の再起動を、2 ユーザーの CPU ジョブと `--no-checkpoint` のジョブで
  行います。起動のたびに、前回の起動のジャーナルが停止順序（最初にデーモンが停止し、ヘルパーは自身が停止する
  前にジョブをダンプし、`--no-checkpoint` のジョブのユニットはヘルパーの後にだけ停止し、最後に
  `/var/lib/gpueue` がアンマウントされる）を示し、タイムアウトや SIGKILL がないこと、`--no-checkpoint` の
  ジョブが SIGTERM を受け取ったこと、リストアしたジョブが止まったところからカウントを続けることを確認します。
  デスクトップに近いホスト：systemd-resolved が動作し、`/tmp` は tmpfs、ユーザーの `allow_other` なしの
  FUSE マウントが `/tmp` の下（AppImage のもののように）とホームディレクトリにあり、実行中と一時停止中の
  ジョブのチェックポイントとリストア、プライベートな `/tmp` に状態を持つジョブ、チェックポイント済み・
  実行中・一時停止中のジョブを抱えた再起動を行います。前のリリースの `.deb`（GitHub からダウンロードし、
  `install.sh` と同じように検証）からの、チェックポイント済みと実行中のジョブを抱えたアップグレード。
  `all` セットは Ubuntu 24.04、Debian 12 と 13（CRIU がないか古すぎるため、インストールとチェックポイント
  なしのジョブのみ）と、`.rpm` を使う Fedora 44 を加えます。
  CI は、ヘルパー、デーモン、チェックポイント、パッケージングに触れるプルリクエストと
  `main` へのすべての push でクイックセット（Ubuntu 26.04）を実行します。リリース自身の `.deb` で
  これが通らない限り、リリースはドラフトとしても作成されません。

まだテストしていないもの：

- 物理マシン（ベアメタル）での実際の再起動と、実際の再起動の GPU に関わる部分：GPU ジョブのチェックポイントと
  リストア、ドライバーの再読み込み、`nvidia-persistenced` に対する停止順序。
- 実際の GitHub リリースからの `install.sh` によるインストール（偽のリリースサーバーに対しては、
  Ubuntu 22.04/24.04/26.04 と Debian 12 のコンテナで `scripts/dist-test.sh` によりテスト済み）。
- 動作中の systemd の下での `.deb` の再インストール、削除、purge（systemd のない素の Debian 12 と Ubuntu の
  コンテナで `scripts/deb-test.sh` によりテスト済み。APT リポジトリのローカルコピーを通したアップグレードも
  `scripts/apt-repo-test.sh` でテスト済み。インストールとアップグレードは上記の VM でテスト済み）、
  systemd の下での Rocky Linux 9 の `.rpm`（インストール、アップグレード、削除は素のコンテナで
  `scripts/rpm-test.sh` によりテスト済み）、実際の `apt` リリースにある APT リポジトリ、実機での arm64。
- カーネルの NFS と autofs の間接マップ（FUSE のネットワークファイルシステムと `x-systemd.automount` は
  テスト済み）。
- マルチ GPU と NCCL のジョブ、テストに使ったもの以外の GPU モデル。

お使いのハードウェアで何が動いて何が動かないか、ぜひ報告してください。
