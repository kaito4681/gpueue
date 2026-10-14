<a id="architecture"></a>
# 架构

[English](../architecture.md) | [日本語](../ja/architecture.md) | 简体中文

<a id="components"></a>
## 组件

| 进程 | 运行身份 | 职责 |
| --- | --- | --- |
| `puzuz`（CLI） | 调用它的用户 | 通过 Unix 套接字上的 HTTP 向守护进程发送请求 |
| `puzuz daemon`（`puzuz.service`） | 系统用户 `puzuz`，沙箱化，无 capability | HTTP API、授权、调度器、SQLite 状态。决定作业何时启动、停止、创建检查点和恢复。 |
| `puzuz-helper`（`puzuz-helper.service`） | root | 以所有者身份启动作业、向其发送信号，并运行 CRIU 和 cuda-checkpoint。只接受来自守护进程用户的连接。 |
| 作业单元（`puzuz-job-<id>.service`，第 n 次恢复后为 `puzuz-job-<id>-r<n>.service`） | 先是 root shim，然后是作业所有者 | 每个作业一个临时（transient）systemd 服务 |

HTTP 服务器和数据库以非特权身份运行。root helper 的依赖树很小（没有 HTTP 服务器，没有 SQLite），
并且自行校验每个请求：uid（绝不能是 0）、以所有者权限打开的工作目录和日志，以及位于其检查点目录下的镜像路径。

<a id="how-a-job-flows"></a>
## 作业的流程

```
puzuz add ──HTTP /v1 over /run/puzuz/puzuz.sock──▶ puzuz daemon ──NDJSON over /run/puzuz/helper.sock──▶ puzuz-helper
  (SO_PEERCRED identifies the caller)                (SQLite, scheduler)                                   │ D-Bus
                                                                                                           ▼
                                                                         systemd: puzuz-job-<id>.service ─▶ shim ─▶ sh -c '<command>'
```

1. **CLI → 守护进程。** CLI 向 `/run/puzuz/puzuz.sock` 发送 `/v1` 下的 REST/JSON 请求（例如 `POST /v1/jobs`），
   并带有 `x-puzuz-version` 请求头。守护进程从 `SO_PEERCRED` 读取调用者的 uid，检查授权并保存作业。
   日志以原始字节流的形式传输，状态变化则以 Server-Sent Events（`/v1/events`）传输。待处理的警告（例如转储失败）
   通过 `x-puzuz-warnings` 响应头返回，由 CLI 打印到 stderr。
   <a id="api-limits"></a>每个用户都能连接到该套接字，因此守护进程会限制单个用户同时保持打开的资源：
   每个 uid 最多 64 个连接和 16 个流（`log --follow`、`/v1/events`），全部连接合计最多 4096 个
   （`puzuz.service` 把 `LimitNOFILE` 设得远高于此）。超出限制的请求会收到 `too_many_requests`。
   连接必须在 20 秒内发送请求头，空闲的 keep-alive 连接会在 20 秒后关闭。`log --tail` 最多只在日志末尾
   4 MiB 内查找行首（如果这一范围内的行数不够，就从那里开始输出），并且同时只运行少量这样的扫描。
2. **调度。** 当排队作业的组有空闲槽位，且其申请的每块 GPU 都有空闲槽位时，该作业启动。
   候选作业按优先级顺序考虑（参见[优先级与公平共享](#priority-and-fair-share)）。守护进程随后请求 helper 创建它。
   立即启动（`start <id>`、`add -i`、`restart -i`、`restore -i`）的作业会在内存中被标记为"强制"。管理员的
   强制启动会忽略所有限制立即启动。其他用户的则纳入调度计划（`scheduler::Reserved::forced`）：先于其他所有
   候选作业考虑，跳过组的 `parallel` 限制和依赖关系，但仍需要其每块 GPU 都有空闲槽位且组未暂停。操作在同一
   事务中检查这一点，因此能放下的启动会立即开始，放不下的会带着原因（`scheduler::LaunchWait`）排队，原因
   包含在响应和作业视图（`waiting`）中。
3. **守护进程 → helper。** 守护进程与 `/run/puzuz/helper.sock` 保持一个连接，交换以换行分隔的 JSON。
   首先由 Hello 消息检查协议版本。之后是带 id 的请求，以及来自 helper 的带编号的事件（作业已退出、检查点或恢复已完成）。
   事件会被持久化，并在守护进程确认之前反复重放，因此守护进程停止期间不会丢失任何内容。
   守护进程会把它最后应用的事件编号与该事件的效果一起保存，并跳过编号不超过它的重放事件，
   因此即使守护进程在确认之前停止，一个事件也只会被应用一次。无法解码的事件（来自更新版本的 helper）
   会被记录到日志并确认，而不会断开连接。重新连接后，双方会核对各自的作业列表：helper 已不再运行的作业视为丢失（lost）；
   守护进程认为正在创建检查点、但 helper 仍在正常运行的作业，说明其检查点请求已丢失，该作业会以检查点失败的状态回到
   运行（或暂停）状态。超时的检查点请求也按同样的方式处理。在因为 helper 不再知道某个作业而把它标记为丢失之前，
   守护进程会先应用 helper 在给出该答复之前发送的所有事件，因此刚刚退出的作业会保留其结果。
   <a id="orphans"></a>如果 helper 正在运行的作业在守护进程看来已完成、正在排队、已创建检查点或根本不存在
   （例如从备份恢复数据库之后），该作业就是*孤立作业*（orphan）。它会继续运行，其 GPU 计为已占用，
   具有相同 id 的排队作业在它结束之前不会启动。`puzuz status` 会向管理员列出孤立作业，`puzuz doctor`
   会对其发出警告，管理员可以用 `puzuz kill <id>` 停止它。
4. **helper → systemd。** helper 通过 D-Bus 启动一个临时服务，其主进程为
   `puzuz-helper shim`。该单元设置了 `Before=puzuz-helper.service` 和 `KillMode=mixed`，stdin 为
   `/dev/null`，停止超时为 `grace_secs`。每个作业都位于自己的 cgroup 中，独立于守护进程、helper 和用户会话，
   因此在守护进程和 helper 重启以及用户注销后仍能继续存在。在 `gpu_device_isolation`（默认开启）时，该单元还设置了
   `DevicePolicy=closed` 和只允许作业 GPU 的 `DeviceAllow=`（见 [GPU 隔离](#gpu-isolation)）。
5. **Shim。** shim 仍以 root 身份运行，它创建一个 PID 命名空间和一个挂载命名空间，并在触发主机的自动挂载点之后
   （以便命名空间获得自动挂载的文件系统）启动该命名空间的 init。init 把挂载设为私有，并挂载一个新的 `/proc`。它会移除被其他挂载遮盖的挂载以及重启后不会保留的挂载
   （`/run/*`、`/media/*`），把每个叠加在其 `autofs` 挂载之上的自动挂载文件系统替换为普通的副本，
   并在 `/tmp` 和 `/dev/shm` 上挂载私有 tmpfs。除非 `job_system_dbus = true`，它会用一个空的 tmpfs 隐藏 `/run/dbus`，并且除非
   `job_systemd_varlink = true`，用只保留 journald 日志套接字、resolved 名称解析套接字和 `resolv.conf` 文件以及 `userdb/`
   的 tmpfs 隐藏 `/run/systemd`，因此即使之后出现新的套接字，作业也无法通过 D-Bus 或 Varlink 向 systemd 或 polkit
   请求任何操作；CRIU 会转储这个挂载并在恢复时
   重建它（见 [作业能做什么、不能做什么](install.md#what-a-job-can-and-cannot-do)）。然后它切换到所有者的
   uid、gid 和组并设置 `no_new_privs`，以所有者身份打开日志
   （`/var/lib/puzuz/logs/<uid>/<id>.log`）和工作目录，并通过 `sh -c` 运行命令。
   环境变量为客户端的环境变量，再加上 `CUDA_VISIBLE_DEVICES`、
   `CUDA_DEVICE_ORDER=PCI_BUS_ID` 以及 `LD_PRELOAD` 中的 fork guard。发给作业的信号（TERM、
   INT、HUP、QUIT、USR1、USR2、TSTP、ALRM、WINCH 以及其他非故障信号）会送到单元的主进程，即外层 shim
   （恢复后则是 init），并被转发给作业的进程组；STOP、CONT 和 KILL 会送到单元的每个进程。helper 会拒绝
   任何其他信号，因为它们会送达 root 的 shim 而不是作业。
6. **检查点。** helper 检查可用空间（用于 RSS 和显存、保存在镜像中的已删除文件，以及作业的 `/tmp` 和
   `/dev/shm` 的内容）、挂载和作业的套接字。作业命名空间中的 FUSE 和网络挂载，如果主机上已没有它，
   或者其 `statfs` 以连接错误失败或在 5 秒内没有返回（在子进程中检查），就被视为已死：在其上有打开的
   文件、工作目录或根目录、可执行文件、映射或绑定的 Unix 套接字的作业，会被拒绝，原因中写明该挂载。
   对其他作业，helper 把它们作为带有自己键名的外部挂载交给 CRIU
   （`--external mnt[<mount point>]:puzuz-detached-<n>`，其下的挂载用 `--skip-mnt`），并记录在
   `helper-mounts.json` 中，这样一个已死的文件系统不会让所有作业都无法做检查点（见
   [不再响应的网络和 FUSE 文件系统](install.md#network-and-fuse-filesystems-that-stop-answering)）。如果检查失败，它会在触碰作业之前拒绝转储，作业继续运行，
   `checkpoint` 请求会失败并给出原因（只有在应当转储时才发现的拒绝，会改为随结果一起报告，并由守护进程警告所有者）。
   在手动模式下，它随后对每个 CUDA 进程运行
   `cuda-checkpoint --action lock` 和 `checkpoint`，接着以 `--tcp-close` 和
   `/var/lib/puzuz/checkpoints/<id>/` 下的镜像目录对命名空间的 init 执行 `criu dump`。
   在插件模式下，它只运行 `criu dump`，使用 CRIU 的默认插件目录，由 CRIU 的 `cuda_plugin.so` 调用 cuda-checkpoint
   （在插件模式下，如果插件缺失，CUDA 作业会被拒绝）。在转储之前，它写入恢复除 CRIU 镜像之外所需的内容
   （所有者记录、作业的挂载和文件、一份 `meta.json` 草稿）；转储成功后立即写入 `DUMPED` 标记，然后是 CRIU
   日志中的外部挂载、最终的 `meta.json`（包含模式），最后写入 `COMPLETE` 标记。转储成功后进程停止，因此从那时起
   镜像永远不会被删除：如果收尾失败，helper 会重试、保留镜像，并报告该检查点尚未完成（守护进程把作业视为已创建
   检查点）；它会在恢复该镜像之前或再次启动时完成该镜像，在转储与标记之间崩溃的情况也是如此，即使 systemd 仍报告被转储的单元处于活动状态。如果转储失败，CRIU 会让进程继续运行，随后由守护进程将其终止。helper 会把 CRIU 的
   `dump.log` 移动到 `/var/lib/puzuz/checkpoints/failed-logs/`，并删除镜像目录。
   **暂停**的作业会在转储之前被继续（SIGCONT），因为 cuda-checkpoint 在已停止的进程上会挂起，因此作业在转储期间
   会运行片刻。守护进程在检查点记录中记下该作业曾处于暂停状态，并要求 helper 以停止状态恢复它（参见第 7 步），
   因此恢复后的作业在 `puzuz start` 之前不会执行任何代码。转储被拒绝时，守护进程会再次暂停它（SIGSTOP）。
   转储进行中时 `pause` 会被拒绝。
7. **恢复。** helper 先占用该作业（此后删除其检查点的请求会被拒绝），然后在其请求循环之外、并把每次文件系统查找
   都放在一个带期限的子进程中进行（以免宕机的 NFS 服务器把它卡住），检查 GPU 型号、驱动、可用显存和挂载：CRIU 要绑定的路径必须存在，作业在转储时所在的文件系统
   （记录在 `helper-mounts.json` 中）必须以相同类型挂载在相同位置（自动挂载点会被触发），
   并且作业曾打开的文件和工作目录必须存在。转储时略去的挂载，如果相同位置再次挂载着同一类型的文件系统
   且它响应，就从主机的该挂载绑定，否则从一个属于 root、权限为 0 的空目录
   （`/var/lib/puzuz/checkpoints/detached-mount`）绑定；其挂载点必须存在。它启动一个新单元 `puzuz-job-<id>-r<n>.service`，在 `/` 的 bind 挂载
   （`/run/puzuz-helper/criu-root`）上运行 `criu restore --root`，并使用恢复所用 GPU 的设备访问权限。在手动模式下，它随后对 CUDA 进程运行 cuda-checkpoint 的
   `restore` 和 `unlock`。在插件模式下，CRIU 的插件会在 `criu restore` 返回之前完成这些操作。
   每个镜像都以其转储时的模式恢复，而不管 `cuda_checkpoint_mode` 现在的设置。作业保留其 id 和日志，镜像随后被删除。
   转储时处于暂停状态的作业会以**停止状态**恢复，与 `puzuz pause` 之后的状态相同：恢复 hook（参见
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

- **在转储时**拒绝在主机文件系统上绑定了 Unix 套接字的作业（CRIU 在恢复时会以 root 身份对这些路径执行绑定、
  unlink 和 chown；作业自己的 `/tmp` 下的套接字没有问题，而绑定到相对名称的套接字会被视为绑定在主机上，因为
  `/proc` 无法说明它在哪个目录中），拒绝带有设置了 `SO_REUSEPORT` 的网络套接字的作业（CRIU 在恢复作业的凭据之前
  以 root 身份创建套接字，因此恢复后的套接字将属于 root，并可能加入同一端口上某个 root 服务的 `SO_REUSEPORT`
  组；已在 CRIU 4.2 上确认），把主机文件系统上已删除的文件保存到镜像中，
  而不是用 `--link-remap` 把它们链接回去（CRIU 会在转储期间以 root 身份按路径执行该操作），并记录作业运行时所属的组；
- **在恢复之前**，如果所有者已离开其中某个组、镜像中有绑定在主机文件系统上的套接字，或者镜像中的某个路径
  （`files.img`）已无法由所有者在途中不经过符号链接而到达（以所有者身份通过
  `openat2(RESOLVE_NO_SYMLINKS)` 检查），则拒绝恢复；
- **在恢复的作业运行之前**，检查 CRIU 实际打开了什么：CRIU 将
  `puzuz-helper verify-restore-hook` 作为其 `pre-resume` 动作脚本（`--action-script`）运行，
  此时恢复的任务已经完整，但仍在其控制下处于停止状态；该 hook 的子进程
  （`puzuz-helper verify-access`）通过 `/proc` 固定（pin）任务的每个打开的文件、工作目录和根目录、
  可执行文件以及文件映射，进入作业的挂载命名空间，切换到所有者身份，并要求所有者能够以它被打开的方式访问其中每一项
  （对固定的文件执行 `faccessat2`），并且能够按其路径、不经过符号链接到达它，且是同一个 inode。
  它还要求每个任务都以所有者身份运行，所属的组是所有者仍在其中的组，并且没有 capability。
  位于作业自身挂载上的文件（`/proc`、私有 tmpfs）来自镜像，会被跳过。对于作业在转储时就已持有的文件，
  只需要其路径通向那个文件本身：作业持有文件的方式可能是其所有者无法再次打开的方式（以只读方式创建，
  或位于把文件显示为 root 所有的 sshfs 挂载上），而取回同一个文件并不会给它带来任何新的权限。
  所持有的文件在转储前列出一次，之后由 CRIU 的 `post-dump` 动作脚本 `puzuz-helper dump-hook`
  在被转储的任务仍处于 CRIU 控制下停止时再列出一次（因此作业在 CRIU 停止它之前刚刚打开的文件也算在内）。
  判断是否为同一文件的依据是：它具有转储时记录的设备号和 inode，或者（重新挂载后，设备号会变化，
  sshfs 等 FUSE 文件系统会重新编号其 inode）具有转储后立即记录的所有者、组、模式、大小、修改时间和变更时间；
  变更时间无法由用户设置，而在该位置重命名或链接另一个文件会改变它（参见
  [文件系统与挂载](install.md#filesystems-and-mounts)）。如果 hook 发现问题，它会在任务
  仍处于停止状态时将其终止（CRIU 本身会忽略失败的 `pre-resume` 脚本），因此这些任务永远不会运行；
  恢复会失败并给出原因，镜像会被保留。如果检查通过且该作业在转储时处于暂停状态，hook 会停止这些任务（SIGSTOP），
  而不是让它们运行。如果 hook 根本没有运行，helper 会在 `criu restore` 返回后立即终止恢复的作业。

最后一项检查查看的是 CRIU 打开的对象，因此通过更改路径与它竞争是没有用的。
剩下的是 CRIU 自身在该检查之前以 root 身份处理路径的短暂窗口：重新创建保存在镜像中的已删除文件
（一个名为 `<name>.cr.<id>.ghost` 的文件，只短暂存在，归作业所有者所有），或创建其路径上缺失的目录。
在预检查与那一刻之间替换目录的所有者，可以让这样的临时文件出现在另一个目录中。

如果 helper 在恢复期间崩溃，它下次启动时会终止恢复单元中运行的任何东西（它未经检查，且在手动模式下其 CUDA 状态可能尚未恢复），
并将该恢复报告为失败（即使该单元已经不存在也是如此）；镜像会被保留。如果 helper 无法记录恢复成功，也会立即终止恢复出的作业并同样处理。

<a id="other-privilege-boundaries"></a>
## 其他权限边界

- <a id="job-credentials"></a>运行谁的作业由 helper 而不是守护进程决定，因此被攻破的守护进程也无法绕过。
  每次启动和恢复时，helper 通过 NSS 查找所有者，并以代码为 `policy_denied` 的错误拒绝 uid 0、gid 0、所有者
  不属于的组、低于 `job_min_uid`（默认为 `/etc/login.defs` 的 `UID_MIN`）的用户、不在非空的
  `job_allowed_users` 中的用户，以及属于 `job_denied_groups`（`docker`、`disk`、`sudo` 等等同 root 的组，在
  `/etc/group` 中查找）的作业组；其消息（`puzuz refuses to run jobs as <user>: <reason>`）会成为作业的启动
  失败原因或恢复错误。作业的附加组由 helper 自己计算（`initgroups` 会设置的组，去掉被拒绝的组和 gid 0）并
  传给 shim；被去掉的组会记录在 helper 的日志中。检查点进程带有被拒绝的组时会拒绝恢复，恢复后的
  检查也只允许所有者的组中去掉被拒绝的组后的那些。见 [谁可以运行作业](install.md#who-jobs-may-run-as)。
- helper 通过一个私有的、归 root 所有的暂存目录（只通过文件描述符使用）把它的套接字绑定到守护进程用户所拥有的
  `/run/puzuz` 中。它一次只服务一个守护进程会话（新会话会取代旧会话），最多允许少量连接等待其 Hello，
  并限制一个会话的队列和进行中请求的数量。
- shim 在降低权限之前以 root 身份处理作业的挂载命名空间。它只通过文件描述符移动和卸离挂载（包括 `/run` 和 `/media` 下的易失挂载），这些文件描述符只经由只有 root
  能修改的目录、且不跟随符号链接地获取一次，因此所有者无法把挂载操作重定向到系统目录上。位于其他人可以修改的
  目录下的挂载会保持不动。附加组由 helper 传入（见上文），因此 shim 不会再次查找。
- 启动时，helper 会在其 `PATH` 中查找一次以名称给出的程序（`criu`、`cuda-checkpoint`、`nvidia-smi`），
  要求找到的文件与以绝对路径给出的程序一样，归 root 所有且只能经由只有 root 能修改的目录到达，此后一直使用
  该绝对路径。
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
  `<data_dir>/criu-no-plugins`，以及 fork guard 的 `<data_dir>/lib`），除非这些目录及其父目录都归 root 所有。

<a id="shutdown-ordering"></a>
## 关机顺序

- **操作系统关机或重启。** systemd 按启动的相反顺序停止单元。
  `puzuz.service` 设置了 `After=puzuz-helper.service`，因此守护进程先停止。停止守护进程永远不会停止作业。
  `puzuz-helper.service` 在本地和远程文件系统、D-Bus、
  `nvidia-persistenced` 和 `nvidia-fabricmanager` 之后运行，因此在它停止期间这些服务仍然可用。
  收到 SIGTERM 时，helper 会询问 systemd 整个系统是否正在停止。如果是，它会拒绝新的工作，
  逐个转储每个运行中的作业（`--no-checkpoint` 作业除外），并不断延长自己的停止超时，直到正在进行的转储的期限结束
  （再加上其回滚时间），然后写入 `shutdown-manifest.json`。每次检查点和恢复都有一个随作业大小而变化的期限；
  超过期限时，该工具的整个进程树会被终止，检查点会被回滚（作业被解冻，其 CUDA 状态被恢复并解锁；
  在插件模式下，被终止的 CRIU 的插件可能让 CUDA 进程停留在锁定或已创建检查点的状态，helper 会通过
  `cuda-checkpoint --get-state` 查询，并按需恢复和解锁它们）。暂停的作业也会被转储
  （先被继续），并在 `puzuz restore` 之后以暂停状态（保持停止，不执行任何代码）回来。作业单元设置了
  `Before=puzuz-helper.service`，因此 systemd 只在 helper 完成之后才停止它们。下次启动时，
  守护进程从 helper 重放的事件中获取结果，并将这些作业标记为
  `Checkpointed`。转储失败或被拒绝的作业已被 systemd 停止。它们被标记为
  `Checkpoint failed` 并附带原因，其所有者会收到警告。
- **`puzuz shutdown`**（管理员）不依赖单元顺序。守护进程逐个为运行中的作业创建检查点
  （或在 `--no-checkpoint` 时终止它们），等待它们稳定下来，然后退出。helper 继续运行。转储失败的作业会被终止。
  转储被 helper 拒绝的作业会继续运行，因为终止它会丢失工作成果。守护进程会记录一条警告，所有者也会收到警告。
  如有需要，请用 `puzuz kill` 或 `puzuz shutdown --no-checkpoint` 停止它。
- 单独执行 **`systemctl stop/restart puzuz-helper` 或 `puzuz`** 时，作业会继续运行。

<a id="priority-and-fair-share"></a>
## 优先级与公平共享

排队作业的顺序是 Slurm 的多因子优先级（公式和设置见
[docs/install.md](install.md#priority-and-fair-share)）。其工作划分与调度器的其他部分相同：

- `puzuz-core::priority` 是纯逻辑的：各因子、公式、所有者自己的顺序（top、`-o` 优先级、id；通过按此顺序
  分配用户可开始作业的等待时间来实现）、经典的 fair-share 因子 `2^(-U/S)`、衰减、作业的计费以及重置周期的边界
  （根据守护进程传入的本地 UTC 偏移计算）。`scheduler::plan_prioritized` 用它对候选作业排序，然后原样应用
  通常的槽位、暂停和依赖规则，因此放不下的高优先级作业永远不会阻塞排在它后面的作业。`plan`/`plan_with`
  保持 pueue 的顺序（也用于 `priority_type = "basic"`）。`scheduler::pending_priorities`（`puzuz prio`
  背后的实现）使用相同的候选选择，因此它列出的正是调度计划所考虑的作业。
- 存储中保存 `jobs.nice`、`jobs.site_factor`、`jobs.top_seq`、`jobs.run_since`、`groups.priority`、
  `user_shares` 和 `user_usage` 表，并在 `meta` 中保存上次计算和重置用量的时间（schema 版本 6）。
- 只有运行时间会被计费。每次状态变化都经过守护进程中的同一个地方（`Core::apply_in_tx`），它在作业开始运行时
  设置 `jobs.run_since`，在作业停止运行时于同一事务中将此后的时间计入其所有者的用量。用量任务每隔
  `priority_calc_period_secs` 运行一次：在一个事务中，它按自上次计算以来的时间衰减每个用户的用量，对运行中的
  作业计费到当前时刻（推进它们的 `run_since`），执行到期的重置，并重新计算 fair-share 因子，守护进程会缓存这些
  因子。调度过程使用该缓存，不读取用量表。一次计费最多覆盖两个周期，因此守护进程停机的时间不会被计费。
- API 增加了 `GET /v1/priority`（等待中的作业及其因子，不含命令，所有人可用）、
  `GET /v1/shares`、`PUT /v1/shares/{uid}` 和 `PUT /v1/groups/{name}/priority`（管理员），
  以及 `POST /v1/jobs/nice`、`/v1/jobs/top` 和 `/v1/jobs/site`，它们像批量端点一样按作业报告结果。
  负的 nice 值、site 因子、份额和组优先级在 `puzuz-core::authz` 中检查。

<a id="state"></a>
## 状态

| 数据 | 位置 | 所有者 |
| --- | --- | --- |
| 作业、组、GPU 槽位上限、检查点记录、待处理的警告、fair-share 的份额和用量 | `/var/lib/puzuz/daemon/state.sqlite`（WAL 并使用 `synchronous=FULL`，已确认的 helper 事件在断电后也不会丢失；用 `PRAGMA user_version` 管理 schema 版本） | 守护进程 |
| 作业记录、事件队列 | `/var/lib/puzuz/helper/` | helper |
| 作业输出 | `/var/lib/puzuz/logs/<uid>/<id>.log` | 目录由 helper 创建（归该用户所有，组为 `puzuz`，模式 2770）。shim 以所有者身份写入日志。守护进程读取和截断日志时不跟随符号链接，并且仅当它是归作业所有者所有的普通文件时才这样做。 |
| 检查点镜像 | `/var/lib/puzuz/checkpoints/<id>/` | helper（仅 root） |
| 配置 | `/etc/puzuz/config.toml` | root |

只有守护进程写入数据库。CLI 始终通过守护进程进行操作。

<a id="crate-layout"></a>
## crate 布局

| Crate | 内容 |
| --- | --- |
| `crates/puzuz` | `puzuz` 二进制文件：兼容 pueue 的 CLI、`puzuz daemon`、`service`、`doctor`、`update`（唯一带有 HTTPS 客户端的 crate） |
| `crates/puzuz-core` | 领域模型、作业生命周期、调度决策、授权、SQLite 存储（没有其他 I/O） |
| `crates/puzuz-protocol` | 通信类型：CLI ⇄ 守护进程（HTTP API 和客户端）、守护进程 ⇄ helper（NDJSON 消息） |
| `crates/puzuz-config` | `/etc/puzuz/config.toml` 的 schema，由守护进程、helper（带 root 所有权检查）和 `doctor` 共用 |
| `crates/puzuz-daemon` | 守护进程：axum API、调度循环、helper 客户端、GPU 检测（NVML） |
| `crates/puzuz-checkpoint` | CRIU 和 cuda-checkpoint 的命令计划、带超时和回滚的执行、镜像布局 |
| `crates/puzuz-helper` | `puzuz-helper` 二进制文件：套接字服务器、作业引擎、systemd 启动器、shim、挂载分析、fork guard（`forkguard/forkguard.c`，嵌入在二进制文件中） |

systemd 单元、sysusers.d 和 tmpfiles.d 片段以及默认配置位于
[`packaging/`](../../packaging) 中，并嵌入到 `puzuz` 二进制文件中，供 `puzuz service install` 使用。
AUR 软件包（`packaging/aur/puzuz-bin`）原样安装相同的文件。
