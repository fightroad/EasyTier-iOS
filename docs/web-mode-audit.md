# Web 模式审计与 issue #42 修复

审计日期：2026-09-20。基线：EasyTier-iOS `79342bd`，内核 EasyTier 2.6.4
(`8428a89d2dabc94c97d370ec607c6ca142473626`)。

覆盖 Rust WebClient hooks / C FFI、Swift PacketTunnelProvider、隧道设置构建、共享
Codable 模型、Dashboard、菜单栏、快捷指令以及 Widget 保存配置与重连路径。

## 根因

2.6.4 的 `NetworkInstanceManager::run_network_instance` 启动后台线程后立即返回。
配置 RPC 在 `instance.run().await` 完成后才注册。旧的 post-run hook 立即通知 Swift；
Swift 调用 `get_config_server_status`，后者同步读取未就绪的 RPC，得到 `options: null`。
Swift 抛出 `Web instance has no tunnel options` 并取消 VPN。由于 String 只实现 Error，
`localizedDescription` 将原文替换成 `Swift.String error 1`。

代码中可以确认该竞态；回归测试覆盖启动前快照、真实内核 RPC 就绪和 Swift 确认顺序。未取得报告者的
iPhone 或完整系统日志，因此不能断言其设备上的被遮蔽错误一定来自这一条分支。

## 已处理的问题

| 优先级 | 问题 | 修复 |
| --- | --- | --- |
| P1 | 实例创建与配置 RPC 就绪不等价 | 保存收到的隧道参数；异步等待 RPC 就绪后才发 run；初始化超时保留明确错误。状态 FFI 只读快照，不持全局模式锁调用同步 RPC。 |
| P1 | 多个独立锁造成 active/pending/error 状态不一致，两个实例可同时通过检查 | 将 Web 状态放到同一互斥锁中，在 pre-run 阶段预留唯一实例；同 ID 覆盖取消旧确认。 |
| P1 | 停止后旧 RPC 可能插入实例，下一次会话复用实例管理器与 generation | 每个 Web 会话使用独立 manager；进程内 generation 单调递增；停止取消确认、终止监控并清理实例；晚到的 post-run 清理旧 manager 的插入。 |
| P1 | run/delete/update 直接并行调用系统网络设置 | Swift 统一串行应用，合并同代 update；OS 操作前后和 TUN 绑定时校验代次；旧会话完成回调不能修改新会话。 |
| P1 | 过期 OS 操作已经写入路由，但旧快照使后续 delete 被误判为无需执行 | 过期完成回调使快照失效，强制后续设置重新核对；测试覆盖“先安装旧地址，再删除到空设置”。 |
| P1 | DHCP 无地址时 force-rebind 绕过地址检查 | 空设置可以成功启动控制连接；有地址后才绑定 TUN，并保留待重绑标记。 |
| P1 | 热更新只重新应用旧的 Swift options | 监控配置、DHCP、代理路由和公网 IPv6 事件；从内核共享的 TomlConfigLoader 刷新 Rust 参数快照，再发 update，避免 NetworkConfig RPC 转换丢失 IPv6。 |
| P1 | retain 删除不调用 post-remove，异常退出不一定关闭事件通道 | 额外监控实例存在性与存活状态；删除清空状态、取消确认；内核/TUN 异常转成明确 error。订阅在 run 回调前建立。 |
| P1 | 服务端断线取消 RPC 等待后，成功确认无法把状态转为 running | Swift 确认独立提交本地状态；不依赖 RPC 接收者仍然存在；监控为被取消的等待补上超时。 |
| P1 | 超时错误排在未完成的系统设置之后，无法取消卡住的启动 | 当前代次 error 绕过普通设置队列，直接通知并取消 VPN；避免重复取消。 |
| P2 | String 错误原文丢失 | 补充 LocalizedError 描述，Rust 日志与 App 错误显示保留原文。 |
| P2 | 无 IPv4 时直接返回，Web 的 IPv6-only 配置无法生成设置 | 独立处理 IPv6 地址、合法前缀和本地 IPv6 子网路由；无任何地址时不提前配置 DNS/MTU。 |
| P2 | Web 模式打开或自动保存本地配置，覆盖 Widget/自动重连的 VPNConfig | 本地编辑仅在 local 模式写 VPNConfig；无指定网络的快捷指令沿用 Web 配置；显式指定本地网络时同步模式。 |
| P2 | 异步状态回复覆盖较新的 Dashboard 状态，辅助界面显示上次本地网络 | Dashboard 校验请求序号；菜单栏读取 Web 网络名与错误；Widget 显示 Web 模式；初始化使用 starting 状态。 |
| P2 | 短 token 中 `?` / `#` 被当成 URL 查询或片段；MTU 被强转 u16 截断 | 使用 URL path segment 编码 token；MTU 保留 u32。修正将上游支持的 HTTPS 发现入口误判为非法的旧测试。 |

跨 FFI 的事件字符串仍由 Swift 在回调返回前复制；返回字符串由 Swift 通过 free_string
释放。内核 TUN 使用 `close_fd_on_drop(false)`，保留 Network Extension 对描述符的所有权。
控制服务器断线本身不删除已经工作的网络实例，仍由上游 WebClient 重连。

## 验证

```sh
cargo test --manifest-path Core/Cargo.toml --target aarch64-apple-darwin --locked --offline
bash tests/run-swift-tests.sh
```

Rust 测试覆盖参数在 RPC 就绪前可读、唯一实例预留、初始化超时、延迟启动、代次与一次性
确认、覆盖取消、停止取消、停止后晚到插入、断线取消等待、配置热更新以及 retain 删除。
其中实例启动 / 路由 patch / retain 用真实 2.6.4 内核运行，无需远程服务器。

Swift harness 编译生产 PacketTunnelProvider 与设置构建代码，替换必须由扩展宿主创建的
基类以及 C FFI，手动交错系统设置回调。覆盖 DHCP、IPv6-only、强制重绑、run/delete
串行化、旧会话回调、过期路由清理和设置回调卡住时的超时取消。测试不安装或启动系统
VPN，不写 App Group 偏好或发出生产 Darwin 通知。

另执行 iOS 和 macOS 完整 Xcode Debug 构建（关闭签名），验证真实 SDK 下的主程序、
Network Extension、共享模型、Widget 和菜单栏编译。

前轮结果：24 项 Rust 测试通过；Swift 生命周期 harness 通过；iOS arm64 与 macOS
arm64/x86_64 完整构建通过；`git diff --check` 通过。构建仍有原有的 CTLIOCGINFO
宏重定义、Debug Rust 库 unwind 大小等警告，未把这些警告当成测试失败或运行验证。

## 上游限制与设备验证范围

EasyTier 2.6.4 的 `InstanceManageRpcService::run_network_instance` 在 post-run hook
返回错误时仅记日志，仍返回 RPC 成功。当前修复能让 Apple 端显示真实失败并取消 VPN，
但不能改变服务器收到的该 RPC 返回值；需要上游修改错误传播契约。

上述测试不等于真机端到端验证。发布前仍应在 iPhone 上按以下流程验证实际隧道权限、
系统 TUN FD 和数据包连通性：

1. 用 issue #42 的 DHCP 配置从 Web 下发，等待地址后验证节点连通；同时验证本地模式。
2. 覆盖同 ID 配置（含地址不变的覆盖）、热更新路由、删除后重新下发。
3. 初始化中停止，再快速启动；确认旧请求不能改变新会话或遗留路由。
4. 断开 / 恢复 Wi-Fi、蜂窝和配置服务器；已运行实例应保持本地状态，控制连接应重连。
5. Web 模式编辑本地配置后，用 Widget / 快捷指令重新连接，仍应使用所选择的模式。
6. 测试静态 IPv6-only 地址与本地 IPv6 子网；跨节点公网 IPv6 路由仍需真实网络验证。

## 生命周期约束与删除竞态

Rust 用 `AwaitingAdmission`、`Reserved`、`Initializing`、`AwaitingAck`、`Running`
表达实例阶段，替代 `initialized` / `running` 两个布尔值。pre-run 只预留 ID；上游在 manager 插入实例后
才调用 post-run，因此只有 post-run 认领后，manager 中不存在实例才代表已删除。

- 初始化期间被 retain 移除：启动等待和监控均检查实际存在性，清空 options 并回到
  `waiting_config`，不会把正常删除误报为超时并取消整个 VPN。RPC 等待被取消后，
  监控仍负责处理删除和超时。
- 晚到的删除回调：上游 post-remove 只有 ID，没有请求 generation。它仅用于核对已
  进入 post-run 的实例，不撤销尚未插入的预留。删除与启动并发时，以 manager 的实际
  插入/删除结果为准；删除后才插入的实例可以继续启动。
- 同 ID 覆盖：上游在 pre-run 前删除旧实例。新代次立即使旧确认失效，但必须等旧
  post-run 消费其 ID 预留后才允许新插入，连续覆盖只放行最新请求。停止、失败和
  超时取消等待；旧回调不能认领新代次。

回归测试覆盖旧删除回调落在新预留之后、插入之后，retain 落在 post-run 之前、
初始化等待取消之后，以及停止中的真实启动等待。旧测试中“post-run 先于插入”的
人工顺序已改为上游真实调用顺序。真实 `InstanceManageRpcService` 覆盖测试继续
验证旧回调消费预留后，新 IPv4 配置能够正常启动。

Swift 使用已有 `settingsApplyGeneration` 表达网络设置应用中，移除重复的
`processingWebEvent`；强制 TUN 重绑只需要保留到成功绑定，因此使用 `needsTunRebind`
布尔标记。现有生命周期测试验证 DHCP 无地址时保留标记、run/delete 串行化、旧会话
回调隔离，以及本地热更新/合并重试失败通知。

## IPv6 与快捷指令选择回归修复

- 保留 pre-run 收到的 `TomlConfigLoader` 共享引用。它与内核运行配置共用存储，
  可以直接读取 DHCP 和 patch 后的字段；启动仍等待实例 RPC 服务就绪才通知 Swift。
  删除有损 `NetworkConfig → gen_config()` 往返以及不再需要的刷新重试状态。
- 新 Rust 集成测试使用真实内核启动 IPv6-only 实例，并连续两次通过真实 patch API
  修改 IPv6 地址及前缀，核对发给 Swift 的状态 JSON，同时验证实例名不被网络名替换。
- Dashboard 监听外部配置选择，只同步编辑会话。保存完成后重新检查模式、配置名和
  会话身份；关闭旧编辑会话不清除新选择或 Web 模式的 `VPNConfig`。
- 将 Dashboard 的配置生命周期方法移到同类型扩展，Swift harness 直接编译这些生产
  方法，使用内存文档和偏好模拟保存/打开的暂停及交错，不写真实 App Group 或 iCloud。
  覆盖旧保存晚到、同名会话替换、外部选择同步、过期打开，以及关闭/激活期间切换选择。

本轮 25 项 Rust 测试通过，Swift 隧道生命周期和配置选择回归测试通过；iOS arm64
与 macOS arm64/x86_64 的无签名 Debug 完整构建通过，diff 检查通过。未进行真机
连通性验证。

## 架构收敛

- Rust 的实例阶段直接携带截止时间。等待旧 post-run、预留插入和初始化各有 10 秒
  窗口，Swift 设置确认有 20 秒窗口；准入等待与后台 monitor 读取同一截止时间。
  post-run 认领后，monitor 负责初始化推进和设置确认超时，RPC 只等待最终结果。
  提交超时前同时校验 generation 和阶段，避免旧阶段快照误伤已运行的实例。
- Swift 用 `WebTunnelSession` 集中持有 Web 阶段、会话参数、实例代次、重绑标记和
  待处理事件。所有访问仍在 `settingsQueue` 上，停止时一次性释放会话状态。
  `starting / ready / failed` 替代可以形成矛盾组合的独立布尔值。
- 事件接收只负责入队与合并；系统设置完成后的队列推进统一归
  `finishNetworkSettingsApply`，过期事件用循环跳过。失败通知与取消归
  `failWebSession`，保证同一会话只取消一次。超时错误仍可绕过未完成的系统设置。
- Rust 回归测试移到 `Core/src/instance/tests.rs`，生产模块保留状态转换和上游接口适配。
  新增“取消 RPC 后超时期限不变”和“阶段切换后的旧超时无效”测试；Swift harness
  覆盖阻塞期间的更新合并、过期事件跳过以及重复错误去重。

本次 27 项 Rust 测试、Swift 隧道生命周期与配置选择测试、iOS arm64 与 macOS
arm64/x86_64 完整无签名 Debug 构建通过；Rust 格式与 diff 检查通过。未进行真机
连通性验证。

## 审查补充修复

- 初始化任务不再依附于配置 RPC。post-run 原子认领实例并登记结果接收者；会话
  monitor 检查内核就绪、订阅事件并发送一次 run。断线取消 RPC 后仍可收到 Swift
  确认并进入 running，未进入 AwaitingAck 的提前确认会被拒绝。
- 手动选择、取消选择和外部同步共用配置替换流程，在首次保存前获取操作标识。
  旧编辑会话保留到新会话准备完成；每次异步操作后核对选择和会话身份，过期的
  打开、保存及失败结果不能覆盖或清空新选择。新建和导入也通过该选择流程激活。
- 切回本地模式时先准备启动配置，成功后才更新模式。没有可用本地编辑会话时
  清除 VPNConfig；保存失败则保留 Web 模式及配置；快捷指令已完成的新选择优先。

新增回归测试覆盖取消初始化 RPC 后正常启动、提前确认拒绝、关闭旧配置时插入
快捷指令、手动选择并发、过期打开失败以及本地模式准备成功、失败和取消。

验证结果：28 项 Rust 测试、Swift 隧道生命周期及配置选择测试通过；iOS arm64、
macOS arm64/x86_64 无签名 Debug 完整构建通过；Rust 格式与 diff 检查通过。
未进行真机连通性验证。

## 事件驱动替代固定轮询

Web 会话不再使用贯穿全生命周期的 250ms sleep 循环。monitor 使用 `select!`
等待状态通知、内核事件和当前阶段截止时间：

- DHCP、配置、路由变化与 TUN 错误直接由事件唤醒处理，不等待轮询周期。
- 空闲、等待下发配置及失败状态没有周期性定时器；停止和新配置通过通知唤醒。
- 预留、准入和 Swift 确认超时按各自截止时间触发；确认与阶段切换会唤醒 monitor，
  不让旧阶段定时器影响新阶段。
- 上游 2.6.4 没有 API 服务就绪通知，因此仅初始化阶段保留 250ms 就绪检查。
- 上游 retain 不调用删除 hook，异常退出也未提供独立可靠的广播通知（stop Notify
  使用 notify_one，已有上游消费者）。运行阶段保留 5 秒生命周期核对；通道关闭或
  删除 hook 到达时立即核对/处理。这个兜底只检查存在性、存活状态，不轮询配置。
  若上游补齐生命周期通知，可以移除这两处兜底检查。

新增测试冻结 Tokio 时间，验证事件处理无需定时器 tick，空闲/失败没有周期唤醒，
以及事件通道未关闭时 retain 删除仍可由兜底检查发现。

## 本地与 Web 共用实例生命周期

配置来源与隧道生命周期分离：

- `Core/src/web.rs` 仅负责服务器地址规范化、WebClient 连接与连接状态，及将
  pre-run / post-run / remove hooks 转交共用实例协调器。
- `Core/src/instance.rs` 的 `InstanceCoordinator` 供两种模式共用，负责单实例
  准入、内核就绪、代次、Swift 设置确认、实时配置快照、事件、超时及退出/删除。
  本地配置也先预留、插入和认领实例，再等待同一个 run / acknowledgement 流程。
- 每次本地或 Web VPN 会话均有独立 manager 与可取消的生命周期任务。删除本地
  专用的全局 running-info / stop callbacks，不再依赖无代次的旧回调处理退出。
- FFI 统一使用 `get_instance_status`、`complete_instance_setup` 和
  `set_instance_tun_fd`。本地 `run_network_instance` 现在也接收实例事件回调。
- Swift 使用一个 `TunnelSession` 和串行事件队列；设置应用、更新合并、代次校验、
  TUN 绑定、失败和停止清理均共用。本地启动等待实例设置成功；Web 控制连接仍可
  先以空设置启动，之后应用服务器下发的实例。
- 本地模式保留宿主选项中的 DNS 覆盖、有效 MTU 和日志级别，地址与路由读取当前
  内核快照。运行中的本地设置更新失败现在也进入统一的错误通知与取消流程。
- 2.6.4 缺少通知的初始化/退出兜底检查仍存在，但属于共用实例层；本次没有升级
  内核依赖，也没有将这些检查重新放回 Web 分支。

回归测试增加真实本地实例的就绪/确认/热更新，实际 FFI 连续启停与过期确认拒绝，
以及 Swift 本地启动等待、选项保留、启动错误一次性完成和旧系统回调隔离。
Swift 测试入口已改名为 `bash tests/run-swift-tests.sh`，生命周期用例在
`tests/TunnelLifecycleTests.swift`。

本轮验证：33 项 Rust 测试、Swift 生命周期及配置选择测试通过；iOS arm64 与
macOS arm64/x86_64 完整无签名 Debug 构建通过；Rust 格式与 diff 检查通过。
未进行真机 VPN 连通性验证。
