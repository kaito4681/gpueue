<a id="architecture"></a>
# 架构

[English](../architecture.md) | [日本語](../ja/architecture.md) | 简体中文

<a id="components"></a>
## 组件

| 进程 | 运行身份 | 职责 |
| --- | --- | --- |
| `gpueue`（CLI） | 调用它的用户 | 通过 Unix 套接字上的 HTTP 向守护进程发送请求 |
| `gpueue daemon`（`gpueue.service`） | 系统用户 `gpueue`，沙箱化，无 capability | HTTP API、授权、调度器、SQLite 状态。决定作业何时启动、停止、创建检查点和恢复。 |
| `gpueue-helper`（`gpueue-helper.service`） | root | 以所有者身份启动作业、向其发送信号，并运行 CRIU 和 cuda-checkpoint。只接受来自守护进程用户的连接。 |
| 作业单元（`gpueue-job-<id>.service`，第 n 次恢复后为 `gpueue-job-<id>-r<n>.service`） | 先是 root shim，然后是作业所有者 | 每个作业一个临时（transient）systemd 服务 |

HTTP 服务器和数据库以非特权身份运行。root helper 的依赖树很小（没有 HTTP 服务器，没有 SQLite），
并且自行校验每个请求：uid（绝不能是 0）、以所有者权限打开的工作目录和日志，以及位于其检查点目录下的镜像路径。

<a id="how-a-job-flows"></a>
## 作业的流程

```
gpueue add ──HTTP /v1 over /run/gpueue/gpueue.sock──▶ gpueue daemon ──NDJSON over /run/gpueue/helper.sock──▶ gpueue-helper
  (SO_PEERCRED identifies the caller)                (SQLite, scheduler)                                   │ D-Bus
                                                                                                           ▼
                                                                         systemd: gpueue-job-<id>.service ─▶ shim ─▶ sh -c '<command>'
```

1. **CLI → 守护进程。** CLI 向 `/run/gpueue/gpueue.sock` 发送 `/v1` 下的 REST/JSON 请求（例如 `POST /v1/jobs`），
   并带有 `x-gpueue-version` 请求头。守护进程从 `SO_PEERCRED` 读取调用者的 uid，检查授权并保存作业。
   日志以原始字节流的形式传输，状态变化则以 Server-Sent Events（`/v1/events`）传输。待处理的警告（例如转储失败）
   通过 `x-gpueue-warnings` 响应头返回，由 CLI 打印到 stderr。
   <a id="api-limits"></a>每个用户都能连接到该套接字，因此守护进程会限制单个用户同时保持打开的资源：
   每个 uid 最多 64 个连接和 16 个流（`log --follow`、`/v1/events`），全部连接合计最多 4096 个
   （`gpueue.service` 把 `LimitNOFILE` 设得远高于此）。超出限制的请求会收到 `too_many_requests`。
   连接必须在 20 秒内发送请求头，空闲的 keep-alive 连接会在 20 秒后关闭。作业操作（`add`、作业动作、批量动作、
   `clean`、`reset`）即使客户端离开也会执行完毕，并且在完成之前一直算作调用者的一个连接，因此被遗弃的操作不会堆积。
   `log --tail` 最多只在日志末尾
   4 MiB 内查找行首（如果这一范围内的行数不够，就从那里开始输出），并且同时只运行少量这样的扫描。单个用户能保存的
   内容也有限制：未结束的作业最多 `max_unfinished_jobs_per_user` 个（超出后 `add` 以及让作业重新运行的 `restart`
   会以 `too_many_requests` 被拒绝），一个作业的命令、工作目录、标签和环境变量合计最多 `max_job_spec_bytes`
   （超出会以 `bad_request` 被拒绝；每个依赖按 8 字节计算，一个作业最多可依赖 1000 个作业）。落后太多的 `/v1/events` 订阅者会收到一个 `resync` 事件来代替它错过的事件；
   `gpueue wait` 随后会重新读取作业列表。
2. **调度。** 当排队作业的组有空闲槽位，且其申请的每块 GPU 都有空闲槽位时，该作业启动。
   候选作业按优先级顺序考虑（参见[优先级与公平共享](#priority-and-fair-share)）。每一轮调度读取未结束的作业时不读取
   它们的命令和环境变量，只完整读取要启动的作业。守护进程随后请求 helper 创建它（同时最多 8 个 spawn 或 restore
   请求；作业在其请求排队等待之前就被记录为运行中）。
   管理员的立即启动（`start <id>`、`add -i`、`restart -i`、`restore -i`）的作业会在内存中被标记为"强制"，
   忽略所有限制立即启动。其他用户的则不是强制的：操作把作业排队（或排队等待恢复），并在同一事务中像
   `gpueue top` 一样把它移到所有者作业的最前面（`Core::queue_first`；在 `enable_user_top = false` 或
   `priority_type = "basic"` 时不移动）。之后调度计划像对待其他排队作业一样对待它；响应中的 `queued` 会说明这一点。
3. **守护进程 → helper。** 守护进程与 `/run/gpueue/helper.sock` 保持一个连接，交换以换行分隔的 JSON。
   首先由 Hello 消息检查协议版本。之后是带 id 的请求，以及来自 helper 的带编号的事件（作业已退出、检查点或恢复已完成）。
   事件会被持久化，并在守护进程确认之前反复重放，因此守护进程停止期间不会丢失任何内容。
   守护进程会把它最后应用的事件编号与该事件的效果一起保存，并跳过编号不超过它的重放事件，
   因此即使守护进程在确认之前停止，一个事件也只会被应用一次。守护进程无法记录的事件（数据库繁忙、已满或出错）不会被确认：
   守护进程会以逐渐延长的间隔重试该事件，并在它被记录之后才应用其后的事件。helper 的 Hello 携带其事件编号的纪元（epoch），
   当 helper 的状态被清除、编号重新开始时纪元会改变；此时守护进程会把 helper 的事件当作新事件应用，而不管其编号。
   （对于不报告纪元的 helper，如果重放在最后应用的编号之前就结束了，就视为这样的重置。）无法解码的事件（来自更新版本的 helper）
   会被记录到日志并确认，而不会断开连接。重新连接后，双方会核对各自的作业列表：helper 已不再运行的作业视为丢失（lost）；
   守护进程认为正在创建检查点、但 helper 仍在正常运行的作业，说明其检查点请求已丢失，该作业会以检查点失败的状态回到
   运行（或暂停）状态。如果该次转储的结果之后仍然到达（helper 在报告转储失败之前会先恢复作业的运行），则会应用它：
   CRIU 尝试过且回滚未完全成功的转储会像其他此类失败的转储一样终止该作业，被拒绝或已干净回滚的转储则不影响该作业。守护进程只有在核对完当前连接之后才会启动作业；
   如果 helper 没有答复获取运行中作业列表的请求，守护进程会再次请求。但如果该检查点请求尚未得到答复，或在上次重新连接之后才得到答复，该列表可能早于这个请求，
   因此守护进程会在答复到达后再检查一次。超时的检查点请求也按同样的方式处理。在因为 helper 不再知道某个作业而把它标记为丢失之前，
   守护进程会先应用 helper 在给出该答复之前发送的所有事件，因此刚刚退出的作业会保留其结果。
   <a id="orphans"></a>如果 helper 正在运行的作业在守护进程看来已完成、正在排队、已创建检查点或根本不存在
   （例如从备份恢复数据库之后），该作业就是*孤立作业*（orphan）。它会继续运行，其 GPU 计为已占用，
   具有相同 id 的排队作业在它结束之前不会启动。`gpueue status` 会向管理员列出孤立作业，`gpueue doctor`
   会对其发出警告，管理员可以用 `gpueue kill <id>` 停止它。
4. **helper → systemd。** helper 通过 D-Bus 启动一个临时服务，其主进程为
   `gpueue-helper shim`，从 helper 二进制文件的内容寻址副本（`<data_dir>/lib/gpueue-helper-<hash>`，见
   [helper 二进制副本](#helper-binary-copies)）运行。该单元的描述只写明作业 id 和所有者的 uid（不含命令行，因为单元描述任何本地用户都能读取，并且会写入日志）。
   它的 capability 边界集只限于 shim 在放弃特权之前所需的那些（`CAP_SYS_ADMIN`、`CAP_SETUID`、`CAP_SETGID`、
   `CAP_SETPCAP`、`CAP_DAC_OVERRIDE`、`CAP_DAC_READ_SEARCH`、`CAP_KILL`）；命名空间 init 启动之后，外层 shim 只保留
   `CAP_KILL`（用于把信号转发给 init，init 随后以所有者身份运行）。该单元设置了 `Before=gpueue-helper.service`、
   `After=remote-fs.target network.target`（仅排序）和 `KillMode=mixed`，stdin 为
   `/dev/null`，停止超时为 `grace_secs`。每个作业都位于自己的 cgroup 中，独立于守护进程、helper 和用户会话，
   因此在守护进程和 helper 重启以及用户注销后仍能继续存在。在 `gpu_device_isolation`（默认开启）时，该单元还设置了
   `DevicePolicy=closed` 和只允许作业 GPU 的 `DeviceAllow=`（见 [GPU 隔离](#gpu-isolation)）。
5. **Shim。** shim 仍以 root 身份运行，它创建一个 PID 命名空间和一个挂载命名空间，并在触发主机的自动挂载点之后
   （以便命名空间获得自动挂载的文件系统）启动该命名空间的 init。init 把挂载设为私有，并挂载一个新的 `/proc`。它会移除被其他挂载遮盖的挂载以及重启后不会保留的挂载
   （`/run/*`、`/media/*`）以及属于其他容器的挂载（位于 `/var/lib/docker/`、`/var/lib/containers/`、
   `/var/lib/containerd/`、`/var/lib/kubelet/pods/` 以及 Docker snap 和 k3s 的对应位置之下；它们随容器出现和消失），把每个叠加在其 `autofs` 挂载之上的自动挂载文件系统替换为普通的副本，
   并在 `/tmp` 和 `/dev/shm` 上挂载私有 tmpfs（在此之前先卸下挂载在那里及其下方的东西，例如主机的 tmpfs
   `/tmp` 以及其上的 AppImage FUSE 挂载，否则它们会隐藏在命名空间中保留下来）。因位于 root 以外的人可以修改的
   目录之下而无法这样够到的挂载（`/tmp` 是普通目录时，用户在 `/tmp/<自己的目录>/` 中的 FUSE 挂载），会在挂载
   私有 tmpfs 之后，像其他被隐藏的挂载一样先把它移到一旁再移除。如果某个被隐藏的挂载其上层的挂载无法用这种方式移到一旁，它会保留
   （该作业随后无法创建检查点）；其他被隐藏的挂载仍会被移除。除非 `job_system_dbus = true`，它会用一个空的 tmpfs 隐藏 `/run/dbus`，并且除非
   `job_systemd_varlink = true`，用只保留 journald 日志套接字、resolved 名称解析套接字和 `resolv.conf` 文件以及 `userdb/`
   的 tmpfs 隐藏 `/run/systemd`，因此即使之后出现新的套接字，作业也无法通过 D-Bus 或 Varlink 向 systemd 或 polkit
   请求任何操作。它同样用空的 tmpfs 隐藏 `job_hidden_socket_dirs` 中的目录（默认为 `/run/libvirt` 和
   `/run/cups`，这些守护进程在用户数据库中检查调用者的组），不存在时先创建；每个目录都只经过只有 root 能修改的
   目录、不跟随符号链接到达（如果某一项途中的目录不属于 root，或可被组或其他人写入，helper 拒绝启动）；CRIU 会转储这些挂载并在恢复时
   重建它（见 [作业能做什么、不能做什么](install.md#what-a-job-can-and-cannot-do)）。然后它切换到所有者的
   uid、gid 和组并设置 `no_new_privs`，以所有者身份打开日志
   （`/var/lib/gpueue/logs/<uid>/<id>.log`）和工作目录，并通过 `sh -c` 运行命令。
   环境变量为客户端的环境变量，再加上 `CUDA_VISIBLE_DEVICES`、
   `CUDA_DEVICE_ORDER=PCI_BUS_ID` 以及 `LD_PRELOAD` 中的 fork guard。发给作业的信号（TERM、
   INT、HUP、QUIT、USR1、USR2、TSTP、ALRM、WINCH 以及其他非故障信号）会送到单元的主进程，即外层 shim
   （恢复后则是 init），并被转发给作业的进程组；STOP、CONT 和 KILL 会送到单元的每个进程。helper 会拒绝
   任何其他信号，因为它们会送达 root 的 shim 而不是作业。init 为了通过 signalfd 接收要转发的信号而
   一直阻塞它们，但启动命令时（通过 `posix_spawn`）不阻塞任何信号，并把每个信号恢复为默认处理方式，
   与登录 shell 或 systemd 服务的初始状态相同，因此 shell 用 exec 运行的程序（`exec python3 train.py`）
   也能收到 SIGTERM 和 SIGINT。gpueue 0.1.2 及更早版本会把被阻塞的信号原样传给作业（见
   [升级](install.md#upgrading)）。
6. **检查点。** 如果配置的 `criu` 在 helper 启动时缺失、无法运行或早于 4.2，helper 首先拒绝所有转储
   （它在启动时运行一次 `criu --version`；没有版本号的输出只会给出警告）：这样的转储会失败并结束作业。然后
   helper 检查可用空间（用于 RSS 和显存、保存在镜像中的已删除文件，以及作业的 `/tmp` 和
   `/dev/shm` 的内容）、挂载和作业的套接字。作业命名空间中的 FUSE 和网络挂载，如果主机上已没有它，
   或者其 `statfs` 以连接错误失败或在 5 秒内没有返回（在子进程中检查），或者它拒绝 root 访问（`lstat`
   以 `EACCES` 失败：未使用 `allow_other` 的 FUSE 挂载），就被视为已死；CRIU 只能从位于作业所有者可以修改的
   目录之下的主机路径重新绑定的挂载也是如此（见 [恢复与路径](#restores-and-paths)）：在其上有打开的
   文件、工作目录或根目录、可执行文件、映射或绑定的 Unix 套接字的作业，会被拒绝，原因中写明该挂载。
   对其他作业，helper 把它们作为带有自己键名的外部挂载交给 CRIU
   （`--external mnt[<mount point>]:gpueue-detached-<n>`，其下的挂载用 `--skip-mnt`），并连同每个挂载点
   是否为文件（文件或套接字的 bind 挂载）、是否位于作业自己的 tmpfs（CRIU 把其内容连同挂载点保存在镜像中）
   一起记录在 `helper-mounts.json` 中，这样一个已死的文件系统不会让所有作业都无法做检查点（见
   [不再响应的网络和 FUSE 文件系统](install.md#network-and-fuse-filesystems-that-stop-answering)）。
   如果命名空间 init 的后代中有进程位于自己的 PID 或用户命名空间（`unshare`、`bwrap`、rootless 容器运行时），
   作业会被拒绝：CRIU 无法转储嵌套的 PID 和用户命名空间。这些检查中对作业文件的每一次查找都在带期限的子进程中进行，
   `checkpoint` 请求的检查在 45 秒后放弃（正在停止的 helper 不再等待它们）。如果检查失败，它会在触碰作业之前拒绝转储，
   作业继续运行，`checkpoint` 请求会失败并给出原因（只有在应当转储时才发现的拒绝，会改为随结果一起报告，并由守护进程警告所有者）。
   CRIU 的 `post-dump` 动作脚本（`gpueue-helper dump-hook`）会在 CRIU 仍停住被转储的任务时，对作业的文件再做一遍相同的检查；
   如果发现应当拒绝的内容（作业在第一轮检查之后打开了它），或没有及时完成，脚本就会失败，CRIU 放弃转储并让作业继续运行，
   检查点会连同原因一起报告为被拒绝。在手动模式下，它随后对每个 CUDA 进程运行
   `cuda-checkpoint --action lock` 和 `checkpoint`，接着以 `--tcp-close`、`--file-locks` 和
   `/var/lib/gpueue/checkpoints/<id>/` 下的镜像目录对命名空间的 init 执行 `criu dump`。
   在插件模式下，它只运行 `criu dump`，使用 CRIU 的默认插件目录，由 CRIU 的 `cuda_plugin.so` 调用 cuda-checkpoint
   （在插件模式下，如果插件缺失，CUDA 作业会被拒绝）。在转储之前，它写入恢复除 CRIU 镜像之外所需的内容
   （所有者记录、作业的挂载和文件、一份 `meta.json` 草稿）；转储成功后立即把镜像刷写到磁盘（CRIU 不会 fsync，而进程已不存在）并写入 `DUMPED` 标记，然后是 CRIU
   日志中的外部挂载、最终的 `meta.json`（包含模式），最后写入 `COMPLETE` 标记。转储成功后进程停止，因此从那时起
   镜像永远不会被删除：如果收尾失败，helper 会重试、保留镜像，并报告该检查点尚未完成（守护进程把作业视为已创建
   检查点）；它会在恢复该镜像之前或再次启动时完成该镜像，在转储与标记之间崩溃的情况也是如此，即使 systemd 仍报告被转储的单元处于活动状态。如果转储失败，CRIU 会让进程继续运行，helper 会回滚 CUDA 状态。如果回滚成功（且 CRIU 没有因期限被终止），helper 会把该失败报告为 `checkpoint_rolled_back`，作业继续运行（如果之前处于暂停状态则再次暂停）；否则由守护进程将其终止。带有守护进程不认识的错误码（来自更新版本的 helper）的转储失败按被拒绝处理：作业继续运行。helper 会把 CRIU 的
   `dump.log` 移动到 `/var/lib/gpueue/checkpoints/failed-logs/`，并删除镜像目录。
   **暂停**的作业会在转储之前被继续（SIGCONT），因为 cuda-checkpoint 在已停止的进程上会挂起，因此作业在转储期间
   会运行片刻。守护进程在检查点记录中记下该作业曾处于暂停状态，并要求 helper 以停止状态恢复它（参见第 7 步），
   因此恢复后的作业在 `gpueue start` 之前不会执行任何代码。转储被拒绝时，守护进程会再次暂停它（SIGSTOP）。
   转储进行中时 `pause` 会被拒绝。
7. **恢复。** helper 先占用该作业（此后删除其检查点的请求会被拒绝），然后在其请求循环之外、并把每次文件系统查找
   都放在一个带期限的子进程中进行（以免宕机的 NFS 服务器把它卡住），检查 GPU 型号、驱动、可用显存和挂载：CRIU 要绑定的路径必须存在，作业在转储时所在的文件系统
   （记录在 `helper-mounts.json` 中）必须以相同类型挂载在相同位置（自动挂载点会被触发），
   并且作业曾打开的文件和工作目录必须存在。转储时略去的挂载，如果相同位置再次挂载着同一类型的文件系统
   且它响应，就从主机的该挂载绑定，否则从一个属于 root、权限为 0 的空目录
   （`/var/lib/gpueue/checkpoints/detached-mount`）绑定，对文件或套接字的挂载则从一个属于 root、权限为 0 的
   空文件（同一位置的 `detached-mount-file`）绑定（因路径看似可被改变而略去的本地文件系统主机路径的绑定，
   只要该路径再次位于同一类型的文件系统上，就视为已恢复）；其挂载点必须存在于主机上（目录或文件），除非它位于
   作业自己的 tmpfs 中。它启动一个新单元 `gpueue-job-<id>-r<n>.service`，在 `/` 的 bind 挂载
   （`/run/gpueue-helper/criu-root`）上运行 `criu restore --root`，并使用恢复所用 GPU 的设备访问权限。在手动模式下，它随后对 CUDA 进程运行 cuda-checkpoint 的
   `restore` 和 `unlock`。在插件模式下，CRIU 的插件会在 `criu restore` 返回之前完成这些操作。
   每个镜像都以其转储时的模式恢复，而不管 `cuda_checkpoint_mode` 现在的设置。作业保留其 id 和日志，镜像随后被删除（如果删除失败，则在作业退出、再次被检查点或 helper 下次启动时删除；残留的恢复源镜像不会导致该作业的下一次转储被拒绝）。
   转储时处于暂停状态的作业会以**停止状态**恢复，与 `gpueue pause` 之后的状态相同：恢复 hook（参见
   [恢复与路径](#restores-and-paths)）在 CRIU 的插件恢复了 CUDA 状态之后、CRIU 仍控制着这些进程时向它们发送
   SIGSTOP，因此它们在执行任何一条指令之前就进入停止状态（CRIU 自带的 `--leave-stopped` 会在插件运行之前就停止
   它们，导致插件失败）。在手动模式下，helper 随后像 CRIU 的插件那样，只让进程的 CUDA 恢复线程运行，并在此期间
   运行 cuda-checkpoint：它用 `ptrace` 附加到这些线程并让它们运行，扣留到达它们的信号（之后会重新发送给该进程，
   因此信号仍保持挂起），然后再次停止这些线程并与之分离；作业的其余部分始终保持停止。它只在这些进程完全停止后
   才附加，并且只附加到作业 cgroup 中这些进程的线程（附加后进行检查）；它从不阻塞等待线程，如果某个线程未能在
   几秒内再次停止，就终止该作业。如果恢复后作业仍有线程在运行，恢复就会失败：作业被终止，镜像被保留。helper 报告该作业已以停止
   状态恢复，守护进程不会向它发送信号。不支持此功能的旧版 helper 仍会让作业继续运行，此时守护进程会停止它
   （SIGSTOP）。
   CRIU 以 root 身份按转储时的路径重新打开作业的文件，因此恢复过程会防范所有者在转储后更改这些路径（参见
   [恢复与路径](#restores-and-paths)）。

<a id="restores-and-paths"></a>
## 恢复与路径

CRIU 以 root 身份运行，并按转储时的路径重新打开、创建或绑定恢复作业的所有内容。转储之后，
所有者可以改变这样的路径所指向的对象，例如把自己的某个目录替换为指向 `/etc` 的符号链接；如果不加防范，
恢复后的作业就会以转储时的模式持有一个 root 的文件。因此 helper 会：

- **在转储时**拒绝其命名空间中有挂载根或挂载点不是 UTF-8、或包含控制字符的挂载的作业（内核在 mountinfo 中只转义
  空格、制表符、换行和反斜杠，而 CRIU 4.2 在任何 ASCII 空白处拆分其字段，并且只反转义空格、制表符和反斜杠，
  因此会在另一个路径上转储这样的挂载），拒绝在主机文件系统上绑定了 Unix 套接字的作业（CRIU 在恢复时会以 root
  身份对这些路径执行绑定、unlink 和 chown；作业自己的 `/tmp` 下的套接字没有问题，而绑定到相对名称的套接字会被
  视为绑定在主机上，因为 `/proc` 无法说明它在哪个目录中）。内核保存的套接字名称与作业传给 `bind(2)` 的完全一致，
  CRIU 也正是绑定这个名称，因此名称只有同时满足以下条件才算作业自己的：它是绝对且已规范化的路径（没有 `.`、`..`
  或空的组成部分），不在 `/proc` 之下，位于某个私有 tmpfs 挂载之下，并且它所在的目录能从该挂载出发、通过从作业根
  目录起的描述符、不经过符号链接、也不跨入其他挂载地到达
  （`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV)`）。它还拒绝有 Unix 套接字在抽象名称上监听的作业
  （CRIU 会以 root 身份设置它们，因此通过 `SO_PEERCRED` 询问谁在监听的客户端会被告知是 root；不监听的抽象套接字没有问题。
  libcuda 的除外：每个初始化了 CUDA 的进程都在 `\0cuda-uvmfd-<其 PID 命名空间的 inode>-<它在其中的 PID>\0`
  （`SOCK_SEQPACKET`）上监听，同一 PID 命名空间中其他进程里的 libcuda 按这个名称连接，用 `SCM_CREDENTIALS` 发送凭据，
  并取得监听方的 `/dev/nvidia-uvm` 描述符；libcuda 从不调用 `getsockopt`，所以 `SO_PEERCRED` 误导不了谁；
  cuda-checkpoint 关闭 `/dev/nvidia*` 描述符时会保留这个套接字。持有它的某个进程映射了 libcuda，且名称中恰好是该进程
  在其 PID 命名空间中的 PID 时放行（dump 之前的扫描和 post-dump 钩子都如此）。不比较 inode：恢复后的进程保留以
  首次运行时的 PID 命名空间命名的监听套接字，而它已处于新的命名空间中。作业也可以这样命名自己的套接字，
  但只有该 PID 命名空间中的 libcuda 会连接它，而它们不看监听方的凭据。恢复后的名称保留 dump 时的 inode，内核以后
  可能把它分给另一个 PID 命名空间：在恢复的作业运行期间，那里 PID 相同的进程无法绑定自己的监听套接字，libcuda 的
  `cuInit` 会以 `CUDA_ERROR_OPERATING_SYSTEM` 失败（反过来，若这样的进程占着该名称，恢复会失败）。主机上任何人本来
  就能占用这样的名称（抽象名称没有权限，作业共享主机的网络命名空间），gpueue 0.1.3 也是这样恢复这些监听套接字的）、
  有 inotify 或 fanotify 描述符的作业（CRIU 会以 root 身份通过跟随符号链接的路径重新添加它们的监视），并且在 post-dump
  钩子中，拒绝带有 Unix 套接字消息中传输中的描述符（`sk-queues.img` 中的 `SCM_RIGHTS`；CRIU 会以 root 身份重新打开
  它们并交给作业，恢复钩子看不到这一过程，因此该钩子会使转储失败，作业继续运行）的转储。转储和恢复都会传入
  `--file-locks`：CRIU 会在恢复作业的凭据之前，以 root 身份在恢复出的描述符上重新获取作业的锁（已在 CRIU 4.2
  中确认）。`flock`、POSIX 和 OFD 锁只取决于描述符的访问模式，因此这不会带来额外权限；如果其间有其他进程持有冲突的锁，
  恢复会等待。文件租约取决于是否拥有该文件（或 `CAP_LEASE`），并且 CRIU 会通过路径打开文件来重新中断正在中断的租约，
  因此 helper 会在转储之前和 post-dump 钩子中拒绝持有租约、委托或这三种之外的锁的作业（与 CRIU 一样从
  `/proc/<pid>/fdinfo/<fd>` 读取）。它拒绝带有设置了
  `SO_REUSEPORT` 的网络套接字的作业（CRIU 在恢复作业的凭据之前
  以 root 身份创建套接字，因此恢复后的套接字将属于 root，并可能加入同一端口上某个 root 服务的 `SO_REUSEPORT`
  组；已在 CRIU 4.2 上确认；helper 通过 `pidfd_getfd` 检查这些套接字，无法检查时拒绝转储），从不使用 `--link-remap`（CRIU 会在转储期间以 root 身份按路径把没有自己名字的打开文件
  链接回其目录）：已删除的文件改为保存到镜像中（作业可能在检查之后创建更大的已删除文件，例如 `/dev/shm` 中的 PyTorch DataLoader
  批数据，因此 CRIU 的 `--ghost-limit` 设为镜像目录所在文件系统的剩余空间）；持有仍有其他硬链接的已删除文件、或持有在打开时被删除而被 NFS
  客户端重命名为 `.nfs<十六进制>` 的文件的作业会被拒绝，因为 CRIU 只能把它们链接回去。恢复时会以 root 身份按原所有者和
  模式重新创建已删除文件，因此持有带 setuid / setgid 位或文件能力（`security.capability` 属性）的已删除文件的作业
  也会被拒绝（否则它会取回一个有特权的程序）。其他已删除文件，无论是否属于所有者（例如作业运行期间被软件包升级
  替换的共享库），都会被保存：其内容是作业本来就能读取的。它还记录作业运行时所属的组。对作业描述符的所有这些检查
  在转储之前进行，并在 CRIU 仍停住作业时由 `post-dump` 脚本再次进行；如果作业在此期间打开了这样的描述符，脚本会否决转储
  （CRIU 恢复作业运行，检查点会连同找到的所有原因一起报告为被拒绝）；
- **在恢复之前**，如果所有者已离开其中某个组、镜像中有绑定在主机文件系统上的套接字（按同样的规则判断其名称）、
  在抽象名称上监听的套接字（libcuda 的除外，只要名称中的 PID 在 `pstree.img` 中且镜像中映射了 libcuda）、inotify 或 fanotify 描述符、传输中的描述符（`sk-queues.img`），或者有绑定到某个端口的
  网络套接字而主机上另一个套接字也占用该端口（任何地址和协议族，相同协议；从 `/proc/net/{tcp,udp,udplite}{,6}`
  读取），就拒绝恢复：CRIU 4.2 会在绑定之前以 root 身份对它恢复的每个网络套接字设置 `SO_REUSEADDR` 和
  `SO_REUSEPORT`，因此它可能加入转储之后占用该端口的某个 root 服务的组。恢复钩子会在 CRIU 绑定套接字之后、作业运行之前
  再检查一次（别人的套接字占用了作业某个套接字的端口，会导致恢复的作业被终止）。镜像中的某个路径
  （`files.img`）已无法由所有者在途中不经过符号链接而到达（以所有者身份通过
  `openat2(RESOLVE_NO_SYMLINKS)` 检查），则拒绝恢复；
- **对 CRIU 从主机绑定的挂载**（作业命名空间的外部挂载，以及代替转储时略去的挂载的主机文件系统）：CRIU 以
  root 身份按路径查找每个来源，并跟随途中的符号链接（`open_tree(AT_SYMLINK_NOFOLLOW)` 只保护最后一个组成部分），
  而恢复钩子只检查任务持有的文件，不检查它们的挂载。因此到这样的来源途中的每个目录都必须是任何作业的所有者都
  无法修改的目录（属于 root 或除守护进程用户以外低于 `job_min_uid` 的系统账户，除非带 sticky 位否则不可被组或
  其他人写入，且不在 FUSE 或网络文件系统上，因为在那里查找返回什么由服务器或其他客户端决定），并且不跟随符号链接地
  检查，来源本身也不能是符号链接（CRIU 会跟随它），在 `/tmp` 这样的 sticky 目录中，它也不能是别人的条目（其所有者可以
  在那里重命名它）或不存在（任何人都可以在那里创建它）；这样在 CRIU 绑定之前作业的所有者就无法改变它。这假定没有作业的所有者
  能在 gpueue 之外以低于 `job_min_uid` 的（守护进程用户以外的）系统账户身份行事（gpueue 从不以这样的账户运行作业）：
  能通过其他手段（`sudo` 规则、自己控制的服务）以这样的账户身份运行代码的所有者可以修改它的目录，因此请让
  `job_min_uid` 高于用户能够冒用的所有账户。没有作业以这样的系统账户运行，因此其目录与 root 的目录一样安全：shim 从中把 resolved 的套接字和 `resolv.conf`
  文件绑定到每个作业的 systemd-resolved 的 `/run/systemd/resolve`（属于 `systemd-resolve`）可以通过（gpueue 0.1.2
  只接受 root 的目录，在每次转储时略去这些绑定，随后拒绝每一次恢复）。否则，例如在自己的主目录中有 `allow_other` FUSE 挂载的所有者，可以在
  检查之后重命名途中的某个目录并在原处放一个符号链接，恢复的作业就会被绑定上另一个主机目录，例如它本来无法到达
  的其他用户主目录下的目录。转储时，这样的挂载被视为已死（使用它的作业被拒绝，其他作业在不包含它的情况下被
  检查点）；略去的挂载除非其主机路径在恢复时通过此检查，否则恢复为空的、无法访问的目录；记录的外部挂载来源未
  通过此检查的恢复会被拒绝；
- **在恢复的作业运行之前**，检查 CRIU 实际打开了什么：CRIU 将
  `gpueue-helper verify-restore-hook` 作为其 `pre-resume` 动作脚本（`--action-script`）运行，
  此时恢复的任务已经完整，但仍在其控制下处于停止状态；该 hook 的子进程
  （`gpueue-helper verify-access`）通过 `/proc` 固定（pin）任务的每个打开的文件、工作目录和根目录、
  可执行文件以及文件映射，进入作业的挂载命名空间，切换到所有者身份，并要求所有者能够以它被打开的方式访问其中每一项
  （对固定的文件执行 `faccessat2`），并且能够按其路径、不经过符号链接到达它，且是同一个 inode。
  它还要求每个任务的每个线程都以所有者身份运行，所属的组是所有者仍在其中的组，没有 capability，capability 边界集为空，
  并设置了 `no_new_privs`，而且命名空间 init 不可转储（dumpable）（它持有 shim 以 root 身份打开的文件，因此其所有者不能
  对它执行 ptrace；CRIU 会恢复转储时的该标志）。
  位于作业自身挂载上的文件（`/proc`、私有 tmpfs）来自镜像，会被跳过。对于作业在转储时就已持有的文件，
  只需要其路径通向那个文件本身：作业持有文件的方式可能是其所有者无法再次打开的方式（以只读方式创建，
  或位于把文件显示为 root 所有的 sshfs 挂载上），而取回同一个文件并不会给它带来任何新的权限。
  所持有的文件在转储前列出一次，之后由 CRIU 的 `post-dump` 动作脚本 `gpueue-helper dump-hook`
  在被转储的任务仍处于 CRIU 控制下停止时再列出一次（因此作业在 CRIU 停止它之前刚刚打开的文件也算在内）。
  判断是否为同一文件的依据是：它具有转储时记录的设备号和 inode，或者（仅在重新挂载使设备号变化后，或在 inode 编号不稳定
  的 FUSE 或网络文件系统上；sshfs 会重新编号 inode）具有转储后立即记录的所有者、组、模式、大小、修改时间和变更时间，
  以及文件系统报告的创建时间；
  变更时间无法由用户设置，而在该位置重命名或链接另一个文件会改变它（参见
  [文件系统与挂载](install.md#filesystems-and-mounts)）。helper 在带期限的子进程中查询这些属性，
  因此无响应的文件系统不会让 helper 卡住（未能在期限内查询到的文件不记录属性，只按设备号和 inode
  判断）。如果 hook 发现问题，它会在任务
  仍处于停止状态时将其终止（CRIU 本身会忽略失败的 `pre-resume` 脚本），因此这些任务永远不会运行；
  恢复会失败并给出原因，镜像会被保留。如果检查通过且该作业在转储时处于暂停状态，hook 会停止这些任务（SIGSTOP），
  而不是让它们运行。如果 hook 根本没有运行，helper 会在 `criu restore` 返回后立即终止恢复的作业。

最后一项检查查看的是 CRIU 打开的对象，因此通过更改路径与它竞争是没有用的。
剩下的是 CRIU 自身在该检查之前以 root 身份处理路径的短暂窗口：重新创建保存在镜像中的已删除文件
（一个名为 `<name>.cr.<id>.ghost` 的文件，只短暂存在，带有已删除文件原来的所有者和模式，转储时已确认它
没有 setuid / setgid 位），或创建其路径上缺失的目录。
在预检查与那一刻之间替换目录的所有者，可以让这样的临时文件出现在另一个目录中。

如果 helper 在恢复期间崩溃，它下次启动时会终止恢复单元中运行的任何东西（它未经检查，且在手动模式下其 CUDA 状态可能尚未恢复），
并将该恢复报告为失败（即使该单元已经不存在也是如此）；镜像会被保留。如果 helper 无法记录恢复成功，也会立即终止恢复出的作业并同样处理。

<a id="other-privilege-boundaries"></a>
## 其他权限边界

- <a id="job-credentials"></a>谁可以运行作业的策略由 helper 而不是守护进程执行，因此被攻破的守护进程也无法绕过该策略：
  它无法让 helper 以 root 身份运行任何东西、加入所有者不属于的组、以策略拒绝的用户身份运行，或以普通用户范围之外的
  uid 运行。helper 无法检查的是谁提交了作业：它从守护进程获取 uid、工作目录、命令和环境变量，因此被攻破的守护进程可以
  以任何通过策略的用户身份启动任何命令（并通过信号、检查点和恢复请求操作这些用户的作业）。客户端的授权（谁可以添加、
  查看或更改哪个作业）由守护进程负责，守护进程保存的作业环境变量也是如此：`gpueue-admin` 的成员可以通过 CLI 读取每个
  用户的作业环境变量（包括其中传入的机密）。由 helper 自己验证提交者的设计尚未实现（issue #107）。每次启动和恢复时，
  helper 通过 NSS 查找所有者，并以代码为 `policy_denied` 的错误拒绝 uid 0、gid 0、高于 `job_max_uid`（若已设置）的 uid、
  65534、65535、4294967295 和 systemd 动态用户范围 61184-65519、所有者
  不属于的组、低于 `job_min_uid`（默认为 `/etc/login.defs` 的 `UID_MIN`）的用户、不在非空的
  `job_allowed_users` 中的用户，以及属于 `job_denied_groups`（`docker`、`disk`、`sudo` 等等同 root 的组，在
  `/etc/group` 中查找）的作业组；其消息（`gpueue refuses to run jobs as <user>: <reason>`）会成为作业的启动
  失败原因或恢复错误。作业的附加组由 helper 自己计算（`initgroups` 会设置的组，去掉被拒绝的组和 gid 0）并
  传给 shim；被去掉的组会记录在 helper 的日志中。检查点进程带有被拒绝的组时会拒绝恢复，恢复后的
  检查也只允许所有者的组中去掉被拒绝的组后的那些。见 [谁可以运行作业](install.md#who-jobs-may-run-as)。
- helper 通过一个私有的、归 root 所有的暂存目录（只通过文件描述符使用）把它的套接字绑定到守护进程用户所拥有的
  `/run/gpueue` 中。它一次只服务一个守护进程会话（新会话会取代旧会话），最多允许少量连接等待其 Hello，
  并限制一个会话的队列和进行中请求的数量。
- shim 在降低权限之前以 root 身份处理作业的挂载命名空间。它只通过文件描述符移动和卸离挂载（包括 `/run` 和 `/media` 下的易失挂载以及其他容器的挂载），这些文件描述符只经由只有 root
  能修改的目录、且不跟随符号链接地获取一次，因此所有者无法把挂载操作重定向到系统目录上。位于其他人可以修改的
  目录下的挂载会保持不动。附加组由 helper 传入（见上文），因此 shim 不会再次查找。
- 启动时，helper 会在其 `PATH` 中查找一次以名称给出的程序（`criu`、`cuda-checkpoint`、`nvidia-smi`），
  要求找到的文件与以绝对路径给出的程序一样，归 root 所有且只能经由只有 root 能修改的目录到达，此后一直使用
  该绝对路径。未找到的程序在 helper 重启之前一直不可用；helper 在其 Hello 中列出它以及已找到程序的路径，
  守护进程再把这些转给 `gpueue doctor`（它会报告之后安装、删除或变得可被替换的程序）。没有 cuda-checkpoint 时，或者某个进程映射了 NVIDIA 设备但 `cuda-checkpoint --get-state`
  无法判断其状态时，GPU 作业的转储会被拒绝，而不是交给 CRIU，因为 CRIU 无法转储设备映射。
- <a id="gpu-isolation"></a>**GPU 隔离。** `CUDA_VISIBLE_DEVICES` 只是告诉 CUDA 使用哪些 GPU，任何作业都
  可以修改它。在 `gpu_device_isolation = true`（默认）时，helper 为每个作业单元（无论启动的还是恢复的）设置
  `DevicePolicy=closed` 和一个 `DeviceAllow=` 列表（读写）：每块所分配 GPU 的 `/dev/nvidia<minor>`，加上
  `/dev/nvidiactl`、`/dev/nvidia-uvm`、`/dev/nvidia-uvm-tools`、`/dev/nvidia-modeset` 和 `char-nvidia-uvm`；
  仅使用 CPU 的作业什么也不允许。systemd 通过 cgroup 设备程序对单元的所有进程（包括 root）强制执行，因此
  打开其他 NVIDIA 设备会以 `EPERM` 失败；`closed` 仍允许标准伪设备（`/dev/null`、`/dev/zero`、`/dev/full`、
  `/dev/random`、`/dev/urandom`、`/dev/tty`、`/dev/ptmx`、`/dev/pts/*`）。每次启动和恢复时，GPU 编号
  （PCI 总线顺序）通过驱动的 `/proc/driver/nvidia/gpus/<PCI address>/information`（`Device Minor`，不含被排除的
  GPU）映射到设备次设备号。恢复单元获得恢复所用的 GPU：`criu restore` 以及插件模式下 CRIU 的 CUDA 插件在其中
  运行，而在手动模式下，当 helper 对被恢复的进程运行 `cuda-checkpoint --action restore` 时，驱动会在这些进程中
  重新打开设备节点。见安装指南中的 [GPU 隔离](install.md#gpu-isolation)。
- 启动时，helper 会拒绝它要写入或从中加载代码的目录（包括传给 `criu -L` 的
  `<data_dir>/criu-no-plugins`，以及存放 fork guard 和 helper 副本的 `<data_dir>/lib`），除非这些目录及其父目录
  都归 root 所有。

<a id="helper-binary-copies"></a>**helper 二进制副本。** 作业的进程树中包含 helper（命名空间的 init 是
`gpueue-helper shim-init`），因此每个检查点镜像都映射了 helper 的可执行文件，而 CRIU 只有在该路径上的文件
大小和构建 ID 相同时才会恢复它。软件包升级、`install.sh` 和 `gpueue update` 会原地替换已安装的二进制文件，
这曾使升级前转储的作业无法恢复，跨越升级运行的作业无法转储（CRIU 无法转储这么大的已删除可执行文件）。
因此 helper 在启动时把自身复制到 `<data_dir>/lib/gpueue-helper-<内容的 FNV-1a 哈希>`（模式 0755，先写入
临时文件再 rename 到位，然后像其他归 root 所有的路径一样检查），作业单元运行这个副本。作业记录以及随镜像
保存的记录（`helper-job.json`）会指明它（`shim_exe`），因此恢复会让进程树回到转储时的二进制文件上，副本
缺失时拒绝恢复并说明原因。恢复之前，helper 还会检查该二进制文件是归 root 所有、且其他人不可写的普通文件，以及副本的内容
与其名称所示一致；启动时，它会像检查其他归 root 所有的路径一样检查自身的二进制文件及其上层目录。helper 每次启动时都会删除既不是它自己的、也没有被作业记录或镜像记录指明、也没有
被镜像（`files.img`）映射的副本；如果有检查点无法读取，则不删除任何副本。在 NixOS 上 helper 从路径不变的
Nix store 运行，不创建副本，而由 Nix GC root（`/nix/var/nix/gcroots/gpueue/job-<id>`）保留旧的 store 路径。
如果无法创建副本，helper 会记录警告，作业单元则运行已安装的二进制文件。在 SELinux 下，在 `/var/lib` 中创建的
文件类型为 `var_lib_t`，systemd（`init_t`）可能无权执行它（作业单元以状态 203/EXEC 失败）。因此 helper 会把
自身二进制文件的 `security.selinux` 属性（`/usr/bin/gpueue-helper` 为 `bin_t`；systemd 正是从它启动了
helper，所以可以执行它）复制到 rename 之前的临时文件上，每次启动时重新给已有副本打标签，并在作业单元或恢复
使用副本之前检查其标签，这也能修复 `restorecon -R /var/lib` 之类的重新打标签。没有 SELinux 时二进制文件没有该
属性，不会设置任何标签；设置标签失败会记录到日志。不安装文件上下文规则：并非所有系统都有 `semanage`，而 helper
自己打标签也适用于 `install.sh` 和 `gpueue service install` 的安装。同一目录下的 fork guard 库保持
`var_lib_t`：作业运行在不受限的域（`unconfined_service_t`）中，可以映射它。

<a id="shutdown-ordering"></a>
## 关机顺序

- **操作系统关机或重启。** systemd 按启动的相反顺序停止单元。
  `gpueue.service` 设置了 `After=gpueue-helper.service`，因此守护进程先停止。停止守护进程永远不会停止作业。
  `gpueue-helper.service` 在本地和远程文件系统、D-Bus、
  `nvidia-persistenced` 和 `nvidia-fabricmanager` 之后运行，因此在它停止期间这些服务仍然可用。
  收到 SIGTERM 时，helper 会询问 systemd 整个系统是否正在停止。如果是，它会拒绝新的工作，
  逐个转储每个运行中的作业（`--no-checkpoint` 作业除外），并不断延长自己的停止超时，直到正在进行的转储的期限结束
  （再加上其回滚时间），然后写入 `shutdown-manifest.json`。整个关机过程受关机目标（`reboot.target` 等）的
  `JobTimeoutSec=` 限制，超过后 systemd 会强制关闭机器；gpueue 的 drop-in 将其从 systemd 的 30 分钟调高到
  2 小时（见 [关机超时](install.md#shutdown-timeouts)）。每次检查点和恢复都有一个随作业大小而变化的期限；
  超过期限时，该工具的整个进程树会被终止，检查点会被回滚（作业被解冻，其 CUDA 状态被恢复并解锁；
  在插件模式下，被终止的 CRIU 的插件可能让 CUDA 进程停留在锁定或已创建检查点的状态，helper 会通过
  `cuda-checkpoint --get-state` 查询，并按需恢复和解锁它们）。暂停的作业也会被转储
  （先被继续），并在 `gpueue restore` 之后以暂停状态（保持停止，不执行任何代码）回来。作业单元设置了
  `Before=gpueue-helper.service`，因此 systemd 只在 helper 完成之后才停止它们；作业单元还设置了
  `After=remote-fs.target network.target`，因此届时仍在运行的作业（`--no-checkpoint` 作业，以及转储失败或
  被拒绝的作业）会在远程文件系统卸载和网络关闭之前停止。下次启动时，
  守护进程从 helper 重放的事件中获取结果，并将这些作业标记为
  `Checkpointed`。转储失败或被拒绝的作业已被 systemd 停止。它们被标记为
  `Checkpoint failed` 并附带原因，其所有者会收到警告。
- **`gpueue shutdown`**（管理员）不依赖单元顺序。守护进程逐个为运行中的作业创建检查点
  （或在 `--no-checkpoint` 时终止它们），等待它们稳定下来，然后退出。helper 继续运行。转储失败的作业会被终止，除非该转储已被干净地回滚。
  转储被 helper 拒绝的作业会继续运行，因为终止它会丢失工作成果。守护进程会记录一条警告，所有者也会收到警告。
  如有需要，请用 `gpueue kill` 或 `gpueue shutdown --no-checkpoint` 停止它。
- 单独执行 **`systemctl stop/restart gpueue-helper` 或 `gpueue`** 时，作业会继续运行。正在停止的守护进程会先把已决定
  启动但尚未发送给 helper 的作业放回队列（否则下一个守护进程会发现这些作业不见了，把它们标记为 lost，并让依赖它们的作业失败），
  并最多等待 10 秒，让 helper 回答已经发送的 spawn 和 restore 请求。

<a id="priority-and-fair-share"></a>
## 优先级与公平共享

排队作业的顺序是 Slurm 的多因子优先级（公式和设置见
[docs/install.md](install.md#priority-and-fair-share)）。其工作划分与调度器的其他部分相同：

- `gpueue-core::priority` 是纯逻辑的：各因子、公式、所有者自己的顺序（top、`-o` 优先级、id；通过按此顺序
  分配用户现在就能启动（组存在且未暂停、GPU 已检测到、依赖已完成）的作业汇总起来的等待时间来实现）、经典的 fair-share 因子 `2^(-U/S)`、衰减、作业的计费以及重置周期的边界
  （根据守护进程传入的本地 UTC 偏移计算）。`scheduler::plan_prioritized` 用它对候选作业排序，然后原样应用
  通常的槽位、暂停和依赖规则，因此放不下的高优先级作业永远不会阻塞排在它后面的作业。`plan`/`plan_with`
  保持 pueue 的顺序（也用于 `priority_type = "basic"`，此时 `priority::basic_order` 让用户的 `-o` 只重排该用户自己的作业）。`scheduler::pending_priorities`（`gpueue prio`
  背后的实现）使用相同的候选选择，因此它列出的正是调度计划所考虑的作业。
- 存储中保存 `jobs.nice`、`jobs.site_factor`、`jobs.top_seq`、`jobs.run_since`、`groups.priority`、
  `user_shares` 和 `user_usage` 表，并在 `meta` 中保存上次计算和重置用量的时间（schema 版本 6），以及上次重置所针对的
  重置周期开始的本地日期。只有当前周期开始于更晚的日期时才会重置，因此 UTC 偏移的变化（夏令时）不会让同一个边界重置两次。
- 只有运行时间会被计费。每次状态变化都经过守护进程中的同一个地方（`Core::apply_in_tx`），它在作业开始运行时
  设置 `jobs.run_since`，在作业停止运行时于同一事务中将此后的时间计入其所有者的用量。用量任务每隔
  `priority_calc_period_secs` 运行一次：在一个事务中，它按自上次计算以来的时间衰减每个用户的用量，对运行中的
  作业计费到当前时刻（推进它们的 `run_since`），执行到期的重置，并重新计算 fair-share 因子，守护进程会缓存这些
  因子。调度过程使用该缓存，不读取用量表。一次计费最多覆盖两个周期，因此守护进程停机的时间不会被计费。
- API 增加了 `GET /v1/priority`（等待中的作业及其因子，不含命令，所有人可用）、
  `GET /v1/shares`、`PUT /v1/shares/{uid}` 和 `PUT /v1/groups/{name}/priority`（管理员），
  以及 `POST /v1/jobs/nice`、`/v1/jobs/top` 和 `/v1/jobs/site`，它们像批量端点一样按作业报告结果。
  负的 nice 值、site 因子、份额和组优先级在 `gpueue-core::authz` 中检查。

<a id="state"></a>
## 状态

| 数据 | 位置 | 所有者 |
| --- | --- | --- |
| 作业、组、GPU 槽位上限、检查点记录、待处理的警告、fair-share 的份额和用量 | `/var/lib/gpueue/daemon/state.sqlite`（WAL 并使用 `synchronous=FULL`，已确认的 helper 事件在断电后也不会丢失；用 `PRAGMA user_version` 管理 schema 版本） | 守护进程 |
| 作业记录、事件队列 | `/var/lib/gpueue/helper/` | helper |
| 作业输出 | `/var/lib/gpueue/logs/<uid>/<id>.log` | 目录由 helper 创建（归该用户所有，组为 `gpueue`，模式 2770）。shim 以所有者身份写入日志。守护进程读取和截断日志时不跟随符号链接，并且仅当它是归作业所有者所有的普通文件时才这样做。 |
| 检查点镜像 | `/var/lib/gpueue/checkpoints/<id>/` | helper（仅 root） |
| 配置 | `/etc/gpueue/config.toml` | root |

只有守护进程写入数据库。CLI 始终通过守护进程进行操作。

<a id="crate-layout"></a>
## crate 布局

| Crate | 内容 |
| --- | --- |
| `crates/gpueue` | `gpueue` 二进制文件：兼容 pueue 的 CLI、`gpueue daemon`、`service`、`doctor`、`update`（唯一带有 HTTPS 客户端的 crate） |
| `crates/gpueue-core` | 领域模型、作业生命周期、调度决策、授权、SQLite 存储（没有其他 I/O） |
| `crates/gpueue-protocol` | 通信类型：CLI ⇄ 守护进程（HTTP API 和客户端）、守护进程 ⇄ helper（NDJSON 消息） |
| `crates/gpueue-config` | `/etc/gpueue/config.toml` 的 schema，由守护进程、helper（带 root 所有权检查）和 `doctor` 共用 |
| `crates/gpueue-daemon` | 守护进程：axum API、调度循环、helper 客户端、GPU 检测（NVML） |
| `crates/gpueue-checkpoint` | CRIU 和 cuda-checkpoint 的命令计划、带超时和回滚的执行、镜像布局 |
| `crates/gpueue-helper` | `gpueue-helper` 二进制文件：套接字服务器、作业引擎、systemd 启动器、shim、挂载分析、fork guard（`forkguard/forkguard.c`，嵌入在二进制文件中） |

systemd 单元、sysusers.d 和 tmpfiles.d 片段以及默认配置位于
[`packaging/`](../../packaging) 中，并嵌入到 `gpueue` 二进制文件中，供 `gpueue service install` 使用。
AUR 软件包（`packaging/aur/gpueue-bin`）原样安装相同的文件。
