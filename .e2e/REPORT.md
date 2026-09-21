# OrbStack bwrap Pool 安全探测报告

日期：2026-09-21（Asia/Shanghai）
Kubernetes context：`orbstack`
受测 feature 提交：`257b5f39d7456eaae7dda1eaba858158dabb8cc6`
feature 父提交/基线：`08c55e063a0a89c390f46410846ba756d5d817ee`
Runtime 镜像：`opensandbox/pool-runtime:bwrap-pool-probe-257b5f39-v10`

所有 Kubernetes 测试对象均使用 `bwrap-e2e-257b5f39` 前缀或
`app=bwrap-e2e-257b5f39` 标签。现有 `client-gateway-pool`、proxy-csi
对象及业务 PVC 均未被修改。

## Pod 安全配置

- `privileged: false`
- `allowPrivilegeEscalation: false`
- capabilities：先 drop `ALL`，仅向可信 execd supervisor 添加
  `SYS_ADMIN`、`SYS_CHROOT`、`SETPCAP` 和 `SETGID`
- supervisor 的 seccomp/AppArmor 为 `Unconfined`，用于允许 bwrap 构造
  沙盒；用户进程使用 execd 内置 seccomp 策略
- `automountServiceAccountToken: false`
- Pool recycle strategy：`Delete`
- 本地 probe/runtime 镜像：`imagePullPolicy: Never`

## 能力矩阵

| 探测项 | 结果 | 证据 |
|---|---|---|
| bubblewrap 与 FD bind | PASS | 长期 runtime 使用固定的 `O_PATH` descriptor，通过 `--bind-fd` 和 `--ro-bind-fd` 启动成功。 |
| `openat2` | PASS | 真实 PVC selector 使用 `RESOLVE_BENEATH`、`RESOLVE_NO_MAGICLINKS` 和 `RESOLVE_NO_SYMLINKS` 打开；Linux rename/symlink 竞态回归测试通过。 |
| mount/PID/IPC/UTS/cgroup namespace | PASS | 所有内部 namespace inode 均与外层容器不同。 |
| nested user namespace | UNSUPPORTED | OrbStack 返回 `bwrap: setting up uid map: Operation not permitted`。 |
| root setpriv 回退 | PASS | Runtime 选择 `uidMode=setpriv`；OrbStack 不支持切换到任意 UID，但本方案要求的真实 UID/GID 0 模式正常。 |
| seccomp | PASS | 用户进程报告 `Seccomp=2`；denylist 中的 syscall 返回 `EACCES`。 |
| Landlock | PASS | 内核 ABI 8；hardening 日志显示 `landlock active`，安装 45 条规则。 |
| overlayfs | PASS | 原有 isolated-session overlay 探测成功；bwrap Pool 使用容器 RW 根目录，本身不依赖 overlay 模式。 |

## Namespace 证据

| Namespace | 外层 inode | 内层 inode | 结果 |
|---|---:|---:|---|
| user | 4026531837 | 4026531837 | setpriv 模式下按预期相同 |
| mount | 4026533556 | 4026533666 | 已隔离 |
| PID | 4026533664 | 4026533669 | 已隔离 |
| IPC | 4026532510 | 4026533668 | 已隔离 |
| UTS | 4026533663 | 4026533667 | 已隔离 |
| cgroup | 4026533665 | 4026533670 | 已隔离 |
| network | 4026532513 | 4026532513 | 按设计共享 |

## 攻击面矩阵

| 攻击项或安全约束 | 结果 | 证据 |
|---|---|---|
| 回退模式真实 UID/GID | PASS | `uid=0 gid=0` |
| inheritable/permitted/effective/bounding/ambient capabilities | PASS | 五组值均为 `0000000000000000` |
| no-new-privileges | PASS | `NoNewPrivs=1` |
| `mount`、`umount2`、`pivot_root`、`chroot` | PASS | 被阻止，errno 13 |
| `setns`、`unshare`、`ptrace`、`bpf`、`keyctl` | PASS | 被阻止，errno 13 |
| 真实 PVC 根目录 | PASS | `/storage` 是空的只读遮罩；project-B 不可见。 |
| 兄弟项目 | PASS | project-A 命令只能读取 `A-SECRET-257b5f39`；兄弟目录查找结果为隐藏。 |
| A/B 双 Sandbox 配对验证 | UNSUPPORTED | 两个 selector 方向均已由 policy/FD 测试覆盖，但本地 lifecycle API 凭证变更后未能完成最终双 Sandbox 在线分配。需要使用有效的生产等价 API 凭证复测。 |
| 只读 selector | PASS | create、chmod 和 rename 均失败；shared 内容仍可读取。 |
| 可写 selector | PASS | 写入成功；删除 probe Pod 后，另一 warm Pool Pod 从 PVC 读到了 `persisted-final`。 |
| 控制面文件 | PASS | execd 二进制不可见；`/run/execd` 和 ServiceAccount 路径为空遮罩；token 不存在；`/proc/1/fd` 不可访问。 |
| `/proc/1/root` 逃逸 | PASS | `/proc/1/root/storage` 不可见。 |
| symlink 与 rename TOCTOU | PASS | symlink selector 被拒绝；目录改名后已打开的 descriptor 仍指向原目录；并发替换为兄弟目录 symlink 时，只会得到原目录 FD 或失败。 |
| 未认证 init | PASS | 在有效初始化前被拒绝，且未消耗 one-shot init 槽位。 |
| 重复 init / 错误 identity | PASS | HTTP 409，未创建第二个 runtime。 |
| 非法 selector、重复 target 或覆盖控制路径 | PASS | server 与 execd 验证测试会拒绝绝对路径、路径穿越、超出 root `maxMode`、重复 target 及控制路径覆盖；父子 target 按深度规范化排序。 |
| 强制模式 isolated session | PASS | Pool runtime 健康时请求被拒绝，避免出现第二条执行路径。 |
| runtime 异常退出 | PASS | 杀死长期 bwrap process group 后，execd 先失效 runtime，再以非零状态结束；任务变为 Failed，旧 Pool Pod 被 Delete recycler 删除并由新 Pod 替换，未回退到外层执行。 |
| 普通 Pool/Sandbox 回归 | PASS | 聚焦 controller 测试及 server 回归测试通过；强制鉴权 header 只会添加到带标记的 bwrap Pool workload。 |

## 业务执行入口

| 执行入口 | 结果 | 说明 |
|---|---|---|
| preStart | PASS | 真实 Pool 分配在 ready 前，将 hardening 探针结果写入选中的 PVC。 |
| 用户 entrypoint | UNSUPPORTED | 一次性测试 Pool 使用 API-only 长期 anchor，没有独立用户 entrypoint 可供实测。 |
| 前台 command | PASS | 观察到相同 namespace 和零 capability 状态。 |
| 后台 command | PASS | 观察到相同的零 capability、NNP 和 seccomp 状态。 |
| PTY | PASS | 通过 WebSocket PTY 观察到相同的零 capability、NNP、seccomp、mount 和 setns 结果。 |
| 文件上传/下载 | PASS | RW 上传与下载成功；兄弟路径返回 404；向 RO 挂载上传返回 `EROFS`。 |
| shell session | PASS | 使用相同的 Pool nsenter wrapper；聚焦 controller/runtime 测试通过。 |
| 周期 lifecycle hook | UNSUPPORTED | 代码通过相同的全局 Pool command wrapper 分发，且 lifecycle 测试通过；但一次性真实 Pool 中未等待并观察定时 hook 执行。 |
| Jupyter kernel | UNSUPPORTED | 一次性 runtime 镜像未包含 Jupyter，无法启动真实 kernel。server entrypoint 与 `/code` 命令路径使用相同的全局 Pool wrapper；需使用生产 Jupyter 镜像复测。 |

## fs-tracker 私有 procfs 复测

- 临时资源前缀：`fs-tracker-proc-e2e-v1`；runtime 镜像固定为
  `opensandbox/pool-runtime:bwrap-fs-tracker-private-proc-v1-full`。
- Pool 使用独立 isolation ConfigMap，仅在强制 `bwrap-v1` 模式启用
  `landlock.allow_private_proc_read=true`；普通 Sandbox 和普通 Pool 不能启用。
- `/ready` 返回匹配的 sandbox identity、generation 1 和 `uidMode=setpriv`。
  bwrap 内用户进程为 `uid=0 gid=0`，五组 capability 全零，
  `NoNewPrivs=1`、`Seccomp=2`，PID 与 mount namespace 均为私有。
- 长期 PID 1 anchor 在 gate 放行前设置 `PR_SET_DUMPABLE=0`。实际验证
  `/proc/1/mem` 和 `/proc/1/environ` 均不可读；没有授予
  `CAP_SYS_PTRACE`，外层 procfs 也没有暴露。
- `fs-tracker doctor --json` 返回 `supported=true`、seccomp listener 与
  proc-mem 后端可用。真实追踪包含子进程，报告为 `finished`、
  `assurance=best_effort`、`gaps=[]`，收到 18 个通知、6 个候选路径并产生
  5 个最终变化，覆盖新增、修改、删除、重命名与子进程写入。
- bwrap 内 `/storage` 和兄弟 project 均不可见；选中 RW 子目录的写入落入
  PVC。`mount`、`unshare` 和读取 PID 1 内存均失败。
- 杀死 bwrap 后任务进入 Failed，旧 Pool Pod 被删除并由新 UID Pod 补充；
  原 Sandbox 未迁移到新 Pod。验证 Pod 能读取此前写入，确认 PVC 数据保留。
- 当前 OrbStack 上已部署的 opensandbox-server 通过公共 Create Sandbox API
  返回 `bwrap-v1 allocation must contain exactly one Pod`。本次因此使用同一
  controller 创建的真实 BatchSandbox 和带认证的 `/internal/init` 完成 runtime
  验证。正式联调前仍需部署包含当前 feature server 代码的镜像，并发布/引用
  包含 `SandboxIsolation` 模型的 Python SDK。
- 全部带 `app=fs-tracker-proc-e2e-v1` 标签的 Pool、BatchSandbox、Pod、PVC、
  Secret 和 ConfigMap 均已删除；现有 `client-gateway-pool` 未发生变化。

## 测试结果与限制

### 嵌套挂载复测

- 临时资源前缀：`bwrap-nested-e2e-v1`；runtime 镜像：
  `opensandbox/pool-runtime:bwrap-nested-v1`。
- `/internal/init` 使用故意乱序的四个 selectors，execd 独立规范化为父挂载先、
  子挂载后；gate 放行前的 source inode、mount ID 和 `ro/rw` 验证通过。
- `rw` 父挂载 `/workspace/rw` 自动创建缺失的
  `cache/content` mountpoint，子挂载覆盖该子树；创建的目录保留在 PVC。
- `ro` 父挂载 `/workspace/ro` 拒绝父目录写入，同时显式授权的
  `rw` 子挂载 `/workspace/ro/cache` 可以写入，且写入落到子 source。
- bwrap 内 `/storage` 继续被遮蔽。外层核对确认两个子挂载写入均保留在
  对应 PVC source，且父只读 source 未被修改。
- 第二个一次性 Pod 使用缺失的 `ro` 父 mountpoint 初始化，返回 HTTP 500，
  `/ready=503`、业务 API=503、PVC 未创建目录，并且没有活跃 bwrap runtime
  或用户 entrypoint。启动探测产生的两个 bwrap zombie 由外层 PID 1 持有，
  与 Pool runtime 泄漏无关。

### runtime 退出与 Pool 回收复测

- 临时资源前缀：`bwrap-recycle-e2e-v1`；runtime 镜像：
  `opensandbox/pool-runtime:bwrap-pool-recycle-v1`。
- 初始化后 `/ready` 返回 `initialized=true`、`uidMode=setpriv`，并通过
  `/command` 将 `after-runtime-init` 写入所选 PVC 子目录。
- 旧 Pod：`bwrap-recycle-e2e-v1-d334310u`，UID
  `9702cec5-851d-4051-86a9-2e6a5ef23b09`。
- 杀死 bwrap process group 后，BatchSandbox 出现 `taskFailed=1`，并写入
  `alloc-release` 与 `alloc-released`；旧 Pod 随后被删除。
- 补充 Pod：`bwrap-recycle-e2e-v1-6lya49mj`，UID
  `69d2ca0d-f1ab-4776-95c2-b280489f699d`。它未被重新分配给失败的
  BatchSandbox，且能从 PVC 读到 runtime 退出前的写入。
- Kubernetes Events 记录了 `PodReleased`、`PodRecycled`、
  `SuccessfulDelete` 和 `SuccessfulCreate`。完整回收用时远低于 60 秒。
- 测试结束后，BatchSandbox、Pool、两个 Pod、每 Pod Secret、PVC 和本地
  临时镜像均已删除；`client-gateway-pool` 仍为 `total=1 allocated=0`。

- 聚焦 Linux Go 测试通过，覆盖 Pool policy 校验、UID mode 选择、FD 固定、
  并发 symlink/rename 攻击、hardening/Landlock、lifecycle control 和强制
  isolated-session 拒绝。
- Kubernetes service 完整测试文件通过：113 个测试全部成功，其中包括
  Pool allocation annotation 解析和业务 token endpoint header。
- execd 全包测试在 arm64 builder 容器中仍存在两个已有的环境相关失败：
  root 用户可以绕过某个测试仅依赖 mode bit 的不可写假设；通用 syscall 名称
  测试在 arm64 上仍要求仅存在于 x86 的 `open` syscall。本 feature 的聚焦测试
  均为绿色。
- OrbStack 结果只验证当前内核与容器 runtime 组合。生产集群仍需运行同一矩阵；
  本方案的威胁模型不包含 Linux 内核漏洞。

## 清理核对

- 已按精确名称或 owner 清理 `bwrap-e2e-257b5f39` Pool、probe/seed Pod、
  每 Pod 控制 Secret 和 PVC。
- 已删除全部本地 `bwrap-pool-probe-257b5f39-*` runtime/server 镜像 tag，
  并停止两个临时 port-forward。
- `opensandbox-server` 已恢复为
  `opensandbox/server:bwrap-pool-257b5f39`，rollout 成功。
- `client-gateway-pool` 及其现有 Pod 的前后状态一致；proxy-csi 和业务 PVC
  未发生修改。
