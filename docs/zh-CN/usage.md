<a id="using-gpueue"></a>
# gpueue 使用指南

[English](../usage.md) | [日本語](../ja/usage.md) | 简体中文

安装 gpueue 之后用它运行作业的参考文档。概览和快速开始见 [README](../../README.zh-CN.md)，安装、配置和运维见
[install.md](install.md)。

- [pueue 兼容性与差异](#pueue-compatibility-and-differences)
- [JSON 输出](#json-output)
- [检查点与恢复](#checkpoint-and-restore)
  - [重启时会发生什么](#what-happens-on-reboot)
- [多用户模型与授权](#multi-user-model-and-authorization)
- [GPU 槽位](#gpu-slots)
- [作业优先级](#job-priority)
- [限制](#limitations)
- [测试范围](#test-coverage)

<a id="pueue-compatibility-and-differences"></a>
## pueue 兼容性与差异

命令名、选项和表格布局遵循 pueue 4.0，因此把 `pueue` 换成 `gpueue` 后，大多数 pueue 的使用习惯和脚本都能继续使用。
gpueue 只复制了 CLI。它不使用 pueue 的协议，也不读取 pueue 的文件。差异如下：

| 项目 | pueue | gpueue |
| --- | --- | --- |
| 守护进程 | 每个用户一个，由用户启动（`pueued`） | 整个系统一个，由 systemd 运行（`gpueue daemon`，用户 `gpueue`）。用户从不启动守护进程。 |
| 选择守护进程 | `-c/--config`、`-p/--profile` | `--socket <path>`（环境变量 `GPUEUE_SOCKET`，默认 `/run/gpueue/gpueue.sock`） |
| `send` | 向任务的 stdin 发送输入 | 不支持：作业的 stdin 是 `/dev/null`。请让命令以非交互方式运行。 |
| `shutdown` | 终止所有任务并停止守护进程 | 仅限管理员。为所有运行中的作业创建检查点，然后停止守护进程。检查点被拒绝的作业会继续运行。`--no-checkpoint` 则改为终止它们。 |
| `restart` | 已结束的任务 | 也适用于 `Checkpointed` 作业：它们作为新作业从头开始，镜像会被保留 |
| `start <id>`、`add -i`、`restart -i`（以及 `restore -i`） | 忽略所有限制，立即启动任务 | 只有管理员的会忽略所有限制立即启动；其他用户的作业会被排到其自己作业的最前面（与 `gpueue top` 相同），并像其他排队作业一样等待轮到它（参见 [作业优先级](#job-priority)） |
| `reset`、`-a/--all`、`-g/--group` | 所有任务 | 你自己的作业。管理员作用于所有人的作业。 |
| `pause/start/kill -a`、`-g` | 同时暂停/继续这些组 | 只有管理员会暂停/继续组；对其他用户而言，组保持原样 |
| 用语 | "task" | "job"。为了兼容，保留了 `--print-task-id` 等选项名。 |
| `--json` | pueue 的内部状态 | gpueue 自己的格式，见下文 |
| `edit`、`env`、`restart --edit` | 支持 | 接受但尚未实现 |
| `add -o/--priority`、`switch` | 对守护进程的所有任务排序 | 只对你自己的作业排序；哪个用户的作业先启动由带 fair share 的 Slurm 风格多因子优先级决定（参见[作业优先级](#job-priority)）。`priority_type = "basic"` 可恢复 pueue 的顺序，但 `-o` 仍然只对你自己的作业排序。 |
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
| `service install [--bin-dir DIR] [--no-start] [--force] / uninstall / status` | 管理 systemd 服务；如果最终检查失败，install 以非零状态退出（参见 [install.md](install.md)） |
| `uninstall [--purge] [--yes]` | 删除服务以及 `service install` 复制的二进制文件（需要 root） |
| `update [--check] [--version V] [-y]` | 将 gpueue 更新到最新（或指定的）发布版本；需要 root，非 root 时只检查（参见 [install.md](install.md#upgrading)） |
| `doctor` | 检查安装情况 |
| `daemon` | 在前台运行守护进程（由 systemd 执行） |

额外的作业状态：`Checkpointing`、`Checkpointed`、`Restore queued`、`Restoring` 以及
`Terminating`（检查点失败后正在停止）。`status` 表格包含 `Owner` 和 `GPU` 列，`Checkpointed` 作业会显示镜像大小。

<a id="json-output"></a>
## JSON 输出

`gpueue status --json` 输出 `{"groups": [...], "jobs": [...]}`：

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
- `gpueue log --json` 输出 `{"<id>": {"job": <job>, "output": "..."}}`。`gpueue group --json`
  输出 `{"groups": [...]}`。`gpueue gpu --json` 输出
  `{"gpus": [{"index", "uuid", "name", "memory_total_bytes", "driver_version", "parallel",
  "used", "jobs": [{"id", "owner"}]}]}`，其中 `"available": false` 表示已消失的 GPU。
- `gpueue prio --json` 输出 `{"priority_type", "weights": {"age", "fairshare", "job_size",
  "partition", "tres"}, "jobs": [{"id", "owner", "group", "priority", "factors": {"age",
  "fairshare", "job_size", "partition", "tres_cpu", "tres_gpu"}, "weighted": {"age", "fairshare",
  "job_size", "partition", "tres"}, "site", "nice"}]}`，作业按调度顺序排列。
  `gpueue share --json` 输出 `{"users": [{"uid", "shares", "norm_shares", "raw_usage",
  "effective_usage", "fairshare", "active"}], "decay_half_life_secs", "calc_period_secs",
  "usage_reset_period", "billing_weights", "usage_calc_at"}`。

<a id="checkpoint-and-restore"></a>
## 检查点与恢复

`gpueue checkpoint <id>` 将运行中（或暂停）的作业保存到磁盘并停止它，从而释放其 GPU；作业随后处于
`Checkpointed` 状态。`gpueue restore <id>` 将它重新排队，一旦其组和 GPU 有空闲槽位就会继续运行
（`restore -i`：排到你自己排队作业的最前面，管理员则立即运行；参见[作业优先级](#job-priority)）。

恢复后的作业从创建检查点的那一刻继续运行。它保留原来的作业 id，其输出会追加到同一个日志中。

暂停的作业同样可以创建检查点，重启时也是如此。它恢复后仍处于暂停状态，并继续占用其 GPU 槽位，用
`gpueue start` 即可让它继续运行。CRIU 和 cuda-checkpoint 需要一个正在运行的进程，因此在转储期间作业会运行片刻。
恢复会让它保持停止状态，因此在 `gpueue start` 之前它不会执行任何代码（哪怕片刻）。检查点正在进行时，
`gpueue pause` 会被拒绝。

<a id="what-happens-on-reboot"></a>
### 重启时会发生什么

1. 操作系统关机时，`gpueue-helper.service` 会在文件系统和 NVIDIA 服务之前停止。它逐个为运行中的作业创建检查点。
   `--no-checkpoint` 作业则直接被终止。
2. 启动后，这些作业显示为 `Checkpointed`。原本在排队的作业继续排队，并照常启动。
3. 每个所有者（或管理员）用 `gpueue restore <id>` 或 `gpueue restore -a` 恢复自己的作业。恢复不会自动开始。

检查点可能以两种方式出错：

- **被拒绝：** 在触碰作业之前，helper 的检查就失败了（可用空间不足、存在一个永远无法恢复的挂载等）。
  作业继续运行。`gpueue checkpoint` 会失败并给出原因。如果拒绝发生在应当转储的时刻（例如在 `gpueue shutdown`
  期间），所有者会在下一次执行 `gpueue` 命令时收到警告。原因会作为作业的最后一条错误显示在
  `gpueue status` 和 `gpueue log` 中。
- **失败：** 转储本身失败（例如超过 CRIU 上限的已删除文件，或作业持有的文件锁）。CRIU 会让作业继续，
  helper 会撤销对作业 CUDA 状态所做的更改；如果这些都成功，作业会继续运行，原因（以
  `checkpoint failed, the job keeps running:` 开头）会作为作业的最后一条错误显示在 `gpueue status` 和
  `gpueue log` 中。如果无法干净地恢复作业（其 CUDA 状态无法恢复，或 CRIU 在期限到达时于转储中途被终止），
  作业会被停止（先发送 SIGTERM，让它有机会保存自己的检查点，`grace_secs` 之后再发送 SIGKILL），并显示为
  `Checkpoint failed` 及其原因。无论哪种情况，所有者都会在下一次执行 `gpueue` 命令时收到警告。

在操作系统关机或重启期间，systemd 会停止所有无法保存的作业，无论其转储是被拒绝还是失败。启动后，它会显示为
`Checkpoint failed` 及其原因，并且所有者会收到警告。断电或内核崩溃时不会保存任何内容。

<a id="multi-user-model-and-authorization"></a>
## 多用户模型与授权

- 守护进程通过内核报告的套接字对端 uid（`SO_PEERCRED`）识别调用者。客户端无法冒充他人。
- 每个作业属于添加它的用户。它以该用户的 uid、gid 和附加组运行，设置了 `no_new_privs`，并丢弃全部 capability。
  命令在添加作业时所在的目录中运行，使用 `gpueue add` 调用时的环境变量。
- root helper 拒绝以下用户的作业：系统账户（uid 小于 `job_min_uid`，默认为 `/etc/login.defs` 的
  `UID_MIN`）、`nobody`、systemd 动态用户的 uid（61184-65519）、设置了 `job_max_uid` 时高于它的 uid、设置了 `job_allowed_users` 时不在其中的用户，以及主组为等同 root 的组的用户
  （`job_denied_groups`，默认为 `docker`、`lxd`、`incus-admin`、`libvirt`、`libvirtd`、`disk`、`sudo`、`wheel`、
  `admin`）。这些组会从每个作业的附加组中去掉，因此即使所有者能使用 Docker 或 sudo，作业也不能
  （helper 的日志会记录去掉了哪些组）。被拒绝的作业以 `gpueue refuses to run jobs as <user>: <reason>` 为原因启动失败。
  见 [谁可以运行作业](install.md#who-jobs-may-run-as)。
- 即使所有者可以成为 root，作业也无法成为 root 或以 root 身份操作：在 `no_new_privs` 下 setuid 程序（`sudo`、
  `su`、`pkexec`）无法获得特权；作业单元无法访问系统 D-Bus，因此无论 polkit 规则授予所有者的组什么权限，
  都无法执行 `systemctl`、`run0`、`machinectl` 或其他经 polkit 授权的操作（`job_system_dbus = true` 可解除），
  也无法访问 `/run/systemd` 下经 polkit 授权的 systemd Varlink 服务（`job_systemd_varlink`），
  以及在用户数据库中检查所有者的组的守护进程套接字（`/run/libvirt` 中经 polkit 授权的 libvirt 套接字、
  `/run/cups` 中的 CUPS 套接字；`job_hidden_socket_dirs`）；
  等同 root 的组会被去掉；只能打开所分配 GPU 的设备节点。恢复之后同样如此。见
  [作业能做什么、不能做什么](install.md#what-a-job-can-and-cannot-do)。
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
  `shutdown`、对其他用户作业执行 `reset`，以及对[孤立作业](architecture.md#orphans)（helper 正在运行、
  但守护进程并不掌握的进程；`status` 会向管理员列出它们）执行 `kill <id>`。与 pueue 一样，`pause` 和 `kill` 会暂停所选的组，
  `start` 会继续它们，而 `pause --wait` 只暂停组，并让运行中的作业运行完毕。与 pueue 不同的是，
  即使组中没有排队的作业，`kill -g/-a` 也会暂停这些组。
  普通用户运行 `pause/start/kill -g` 或 `-a` 时，只有其自己的作业受影响，任何组的暂停状态都不会改变
  （此时 `pause --wait` 什么也不做）。输出会说明发生的是哪种情况
  （`All groups are being paused.` 或 `Your jobs are being paused: 3, 4`）。
- **只有管理员能立即启动作业**（`start <id>`、`add -i`、`restart -i`、`restore -i`），并忽略所有限制；
  其他用户的作业会被排到其自己作业的最前面（参见 [作业优先级](#job-priority)）。
- 管理员是 root 以及 `gpueue-admin` 组的成员（主组或附加组均可）。每次请求都会进行检查，因此
  `usermod -aG gpueue-admin <user>` 会立即生效。

<a id="gpu-slots"></a>
## GPU 槽位

- `gpueue add --gpu 0,1 ...` 只有在其组有空闲槽位且所列的**每一块** GPU 都有空闲槽位时才会启动作业。
  守护进程会拒绝它不认识的 GPU 编号。
- 默认情况下每块 GPU 同时只允许 1 个作业。管理员可以用
  `gpueue gpu parallel 2`（所有 GPU）或 `gpueue gpu parallel 2 --gpu 1` 更改。`0` 表示不限制。
  槽位按作业计数，而不是按显存。
- GPU 编号遵循 PCI 总线顺序，与 `nvidia-smi` 和
  `CUDA_DEVICE_ORDER=PCI_BUS_ID` 的顺序相同。在作业内部，`CUDA_VISIBLE_DEVICES` 列出所申请的
  GPU，因此程序看到的是 `cuda:0`、`cuda:1`……
- **GPU 隔离：** 作业只能打开分配给它的 GPU 的设备节点。每个作业单元都设置了 systemd 的
  `DevicePolicy=closed`，并用 `DeviceAllow=` 只允许所分配 GPU 的 `/dev/nvidia<N>` 和共用的 NVIDIA 节点；
  仅使用 CPU 的作业完全不能打开 NVIDIA 设备。修改 `CUDA_VISIBLE_DEVICES` 或打开其他 GPU 的节点都会以
  `EPERM` 失败，作业内的 `nvidia-smi` 只显示自己的 GPU（在仅使用 CPU 的作业中会以
  `Failed to initialize NVML: Unknown Error` 失败）。管理员可以用 `gpu_device_isolation = false` 关闭它，
  例如使用 MIG 时（见 [GPU 隔离](install.md#gpu-isolation)）。
- 组的行为与 pueue 相同（`default` 组初始为 `parallel 1`）。两种限制同时生效。
- GPU 槽位、组的 `parallel` 限制和暂停的组适用于普通用户启动的每个作业，使用 `start <id>`、`add -i`、
  `restart -i` 或 `restore -i` 时也一样（参见 [作业优先级](#job-priority)）；只有管理员的即时启动会忽略它们。
- GPU 在守护进程启动时通过 NVML 检测（配置中的 `[[gpus]]` 列表只是后备）。添加或移除 GPU 后请重启
  `gpueue.service`。`gpueue doctor` 会将守护进程检测到的内容与 `nvidia-smi` 进行比较。

<a id="job-priority"></a>
## 作业优先级

当多个作业等待相同的槽位时，在能够放入的作业中优先级最高的那个先启动。优先级遵循 Slurm 的 multifactor
插件：默认情况下，它是 fair-share 因子（最近使用机器较少的用户优先，用量以 7 天的半衰期衰减）与
等待时间因子（等待的时长）之和，再减去作业的 nice 值。也可以加入作业大小、组（partition）和按 GPU（TRES）
的因子。你自己的 `-o` 优先级、`switch` 和 `gpueue top` 只会重新排列你自己的作业。非管理员使用 pueue 的
即时启动（对排队中或暂存作业的 `start <id>`、`add -i`、`restart -i`、`restore -i`）时也是如此：作业会像
`gpueue top` 一样被排到你自己作业的最前面，之后在组的 `parallel` 限制、GPU 槽位、暂停的组和依赖之下与其他
用户的作业竞争；命令会说明它是被排队而不是被启动。在 `enable_user_top = false`（或
`priority_type = "basic"`）时，作业只是被排队。对暂停作业执行 `start` 仍然是恢复它。管理员的即时启动会
忽略所有限制立即启动。`gpueue prio` 显示作业
为何处于当前位置，`gpueue share` 显示所有人的用量。公式和设置参见
[优先级与公平共享](install.md#priority-and-fair-share)。

<a id="limitations"></a>
## 限制

- **网络连接：** 为作业创建检查点时，已建立的 TCP 连接（连接到数据集服务器、数据库、实验跟踪服务等）
  会被关闭（`criu --tcp-close`）。恢复后程序会看到连接已断开，必须重新连接。
- **私有的 `/tmp` 和 `/dev/shm`：** 每个作业在 `/tmp` 和 `/dev/shm` 上都有自己的空 tmpfs。
  它会保存在检查点中，不与其他作业或你的 shell 共享。在重启后不会保留的挂载（`/run` 和 `/media` 下的任何内容）
  以及其他容器的挂载（`/var/lib/docker/` 之下等）在作业内部不可见。请使用持久化文件系统上的路径来交换文件。
- **挂载在启动时固定：** 作业看到的是它启动时已挂载的文件系统（自动挂载点会在那时被触发），而不是之后才挂载的文件系统。
  要恢复作业，这些文件系统都必须重新挂载到相同的路径；否则恢复会被拒绝，镜像会被保留。
  `/scratch` 这类 tmpfs 上的文件在重启后不会保留。参见
  [文件系统与挂载](install.md#filesystems-and-mounts)。
- **没有崩溃保护：** 只有在有序关机、`gpueue shutdown` 和 `gpueue checkpoint` 时才会创建检查点。
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
  [恢复与路径](architecture.md#restores-and-paths)）。
- **Unix 套接字：** 如果作业的 Unix 套接字绑定在其自己的 `/tmp` 和
  `/dev/shm` 之外的路径上（例如你的主目录中），或绑定到 `ctl.sock` 这样的相对名称，就无法为它创建检查点；
  检查点会被拒绝，作业继续运行。请把套接字绑定到 `/tmp` 下的绝对路径（途中不含 `..` 或符号链接），或对不监听的
  套接字使用抽象名称。在抽象名称上*监听*的套接字无法创建检查点（CRIU 会以 root 身份设置它，其客户端会看到
  root 是监听者），但 libcuda 在每个 CUDA 进程中监听的套接字（`@cuda-uvmfd-<PID 命名空间的 inode>-<PID>@`）除外。
- **inotify、fanotify 和传输中的描述符：** 如果作业有 inotify 或 fanotify 描述符（文件监视器），或在创建检查点的
  那一刻正通过 Unix 套接字传递描述符（`SCM_RIGHTS`），就无法为它创建检查点（CRIU 会以 root 身份重新创建它们）。
- **监听端口：** 如果作业的 TCP 或 UDP 套接字绑定了某个端口，仍可为它创建检查点（Jupyter、TensorBoard），但只有在
  主机上没有其他套接字占用该端口时才会恢复；否则恢复会被拒绝，待端口空闲后可以重试。
- **`SO_REUSEPORT`：** 如果作业有设置了 `SO_REUSEPORT` 的 TCP 或 UDP 套接字，同样无法为它创建检查点
  （CRIU 会把该套接字恢复为属于 root 的套接字，它随后可能与某个 root 服务共享端口）。有些服务器默认会设置它；
  请将其关闭，或使用 `--no-checkpoint`。
- **已删除的文件：** 作业在仍打开时删除的文件（临时文件、multiprocessing 信号量、`/dev/shm` 中的 PyTorch DataLoader
  批数据）只要能放进检查点目录的剩余空间，无论多大都会保存到镜像中。如果作业持有
  仍有其他硬链接的已删除文件、在打开时于 NFS 上被删除的文件（`.nfs<十六进制>`），或带有 setuid / setgid
  位或文件能力的已删除程序，就无法为它创建检查点；检查点会被拒绝，作业继续运行。作业运行期间被软件包升级
  替换的库没有问题。
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
  gpueue 会拒绝转储并告诉你原因。作业继续运行。
- 只要作业还存在，检查点镜像就不会被自动删除。在作业成功恢复、被删除（`remove`、`clean`、`reset`）
  或其转储失败时，镜像会被删除。超过 `checkpoint_size_limit_bytes` 时，gpueue 会发出警告，但不会拒绝。
- 暂不支持：`edit`、`env`、定期检查点、在保持作业运行的同时创建检查点、不做完整检查点而释放 GPU
  （"GPU suspend"）、Web UI 或 TCP API，以及按显存计数的槽位。

<a id="test-coverage"></a>
## 测试范围

目前已测试：

- 每个 crate 的单元测试和集成测试（`scripts/test-in-docker.sh`）。
- 在特权 Docker 容器中进行的全栈端到端测试：systemd 作为 PID 1、CRIU，
  二进制文件通过 `gpueue service install` 安装（`scripts/e2e-test.sh`）。测试涵盖基本的队列操作、
  三个用户的授权、组和 GPU 限制、检查点/恢复、守护进程和 helper 的重启、一次性添加大量作业、
  `gpueue shutdown`，以及模拟重启：用 systemd 关闭容器，并在新容器中恢复作业，其中也包括在独立的 ext4 文件系统、
  bind 挂载、`x-systemd.automount`、overlayfs、tmpfs 和 sshfs 上工作的作业，这些挂载会作为新的挂载重新出现。
- 在 Docker 中使用真实的 NVIDIA GPU（手动模式的 CRIU、PyTorch）：
  CUDA 计数器以及使用 fork 出的 DataLoader worker 的 PyTorch 训练的检查点/恢复，
  其结果与未中断的运行完全相同；以及守护进程在打包好的、沙箱化的 `gpueue.service` 中通过 NVML 检测 GPU。
- QEMU/KVM 虚拟机中的真实操作系统（`scripts/vm-reboot-test.sh`）：从各发行版的云镜像启动，并以用户的方式
  安装 gpueue：用 `apt install ./gpueue_<version>_amd64.deb` 安装 `.deb`（在 Fedora 上用 `dnf` 安装
  `.rpm`），或者用 `gpueue service install`，`/var/lib/gpueue` 位于独立的 ext4 磁盘上。在快速集合的
  发行版上，使用两个用户的 CPU 作业以及一个 `--no-checkpoint` 作业，测试
  `systemctl reboot`、`systemctl poweroff` 后重新启动，以及在一个 1 GiB 作业的 `gpueue checkpoint` 仍在进行时
  重启。每次启动后，上一次启动的 journal 必须显示正确的停止顺序（守护进程最先停止，helper 在自身停止之前转储
  作业，`--no-checkpoint` 作业的 unit 只在 helper 之后停止，最后卸载 `/var/lib/gpueue`），且没有超时或
  SIGKILL；`--no-checkpoint` 作业必须收到了 SIGTERM；恢复后的作业必须从停止处继续计数。类桌面主机：
  systemd-resolved 在运行，`/tmp` 是 tmpfs，用户不带 `allow_other` 的 FUSE 挂载位于 `/tmp` 下（如 AppImage
  的挂载）和家目录中；测试运行中和已暂停作业的检查点与恢复、一个在私有 `/tmp` 中保存状态的作业，以及带着已检查点、
  运行中和已暂停作业的重启。从上一个发布版本的 `.deb`（从 GitHub 下载，并像 `install.sh` 那样校验）升级，
  期间有已检查点和运行中的作业。`all` 集合还加入其他 Debian 和 Ubuntu 版本（其 CRIU 缺失或过旧：只测试
  安装和不做检查点的作业）以及使用 `.rpm` 的 Fedora；`scripts/vm-reboot-test.sh list` 会列出这两个集合。
  CI 在涉及 helper、
  守护进程、检查点或打包的拉取请求上以及每次推送到 `main` 时运行快速集合；除非它在该发布版本
  自己的 `.deb` 上通过，否则不会创建发布版本，连草稿也不会。

尚未测试：

- 物理机（裸机）上的真实重启，以及真实重启中与 GPU 相关的部分：GPU 作业跨重启的检查点与恢复、驱动重新加载，
  以及相对于 `nvidia-persistenced` 的停止顺序。
- 从真实的 GitHub Release 用 `install.sh` 安装（已在 Debian 和 Ubuntu 容器中针对伪造的发布服务器测试，
  `scripts/dist-test.sh`）。
- 在运行着 systemd 的环境中 `.deb` 的重新安装、卸载和清除（已在不带 systemd 的普通 Debian 和 Ubuntu 容器中测试，
  `scripts/deb-test.sh`；通过 APT 仓库的本地副本进行升级也已测试，`scripts/apt-repo-test.sh`；安装和升级已在
  上述虚拟机中测试），在 systemd 下 Rocky Linux 9 上的 `.rpm`（安装、升级和卸载已在普通容器中测试，
  `scripts/rpm-test.sh`），真实 `apt` 发布版本中的 APT 仓库，以及真实硬件上的 arm64。
- 内核 NFS 和 autofs 间接映射（FUSE 网络文件系统和 `x-systemd.automount` 已测试）。
- 多 GPU 和 NCCL 作业，以及测试所用型号以外的 GPU 型号。

请报告在你的硬件上哪些功能可用、哪些不可用。
