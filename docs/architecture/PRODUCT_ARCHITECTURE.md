# NebulaDesk：产品工程与运行时架构

本文从产品界面、代码目录和实际进程说明 NebulaDesk 的组成。
协议细节见 [NEBULA_V2.md](NEBULA_V2.md)，桌面调用契约见
[DESKTOP_CONTRACT.md](DESKTOP_CONTRACT.md)，界面基线见
[四张产品设计稿](../../design/product-ui/README.md)。

## 1. 产品由什么组成

用户安装一个桌面产品，使用两类窗口：管理主窗口和独立的远程 Session。
同一台电脑既可以连接别人的资源，也可以运行 Agent，把自己的资源提供出去。
这两个角色互不要求：只做客户端不必开启共享，只做被控机不必保持人工登录。

服务器侧有三个职责，而不是一台包办所有事情的“中转管理服务器”。

| 组件 | 所在位置 | 职责 | 不负责什么 |
| --- | --- | --- | --- |
| Manager | 自建服务器或云端 | 身份、租户、设备、资源、授权、短期会话票据、审计 | 不转发桌面媒体 |
| PostgreSQL | Manager 的受控网络内 | 持久化用户、设备关系、授权及会话记录 | 不对桌面客户端开放 |
| Gateway | 端点可以访问的服务器 | QUIC 接入、Agent 反向控制连接、票据校验、会话生命周期 | 不解码媒体，不是管理页面 |
| Relay | 端点可以访问的服务器 | 转发端到端加密的数据；直连不可用时承载会话 | 不读取桌面、声音、文件或输入 |
| Desktop host | 用户电脑 | 管理 WebView、Manager API、本机操作、Session 子进程 | 不编解码视频 |
| Native Client | 用户电脑，每个 Session 一个进程 | QUIC、解密、原生解码、画面渲染、输入、音频及传输 | 不承载账号管理页面 |
| Agent | 被控电脑 | 注册、采集、编码、输入注入及双向传输 | 不持有人类账号密码 |

小规模部署可把 Manager、Gateway、Relay 放在同一台服务器上，但仍是不同进程、
不同接口。扩容时先按实际负载拆分 Relay；它消耗带宽，Manager 主要消耗数据库和
授权处理资源。当前不需要为了部署方便，把视频绕进 Manager。

## 2. 工程目录与前后端边界

```text
apps/
  desktop-ui/                 React + TypeScript：页面、组件、样式、typed bridge
  desktop-host/               固定版本的桌面打包工具入口，不承载界面代码
crates/
  nebula-desktop/              Rust + Tauri：桌面应用服务与进程管理
    agent-host/                独立的 nebula-desktop-agent 本机共享进程
  nebula-desktop-protocol/     原生 Session 的版本化进程 IPC 类型
  nebula-client/               winit + wgpu：独立 Session、CLI、原生媒体播放
  nebula-agent/                被控服务与 Windows/macOS/Linux 平台后端
  nebula-manager/              axum API、授权、数据库迁移及审计
  nebula-gateway/              QUIC 接入及控制面
  nebula-relay/                密文转发
  nebula-common/              共享标识、策略和控制面类型
  ndp-proto/                  线路消息格式
  ndp-crypto/                 端到端加密和防重放
  ndp-transport/              QUIC 通道、优先级和多路径
  ndp-signal/                 会话建立与信令
design/product-ui/            已确认的四张 SVG 和界面说明
docs/architecture/            产品架构、协议设计和桌面契约
legacy/                      历史实现，不作为新桌面前端依赖
```

```mermaid
flowchart TB
  subgraph Presentation["界面层"]
    React["React 管理页面<br/>资源 / 本机共享 / 详情 / 传输"]
    Chrome["原生 Session 工具条"]
  end
  subgraph Application["桌面应用层"]
    Host["Tauri Rust host<br/>账号 / API / 本机服务 / 进程"]
    Native["Native Client<br/>winit 主线程事件循环"]
  end
  subgraph Media["会话与媒体层"]
    Session["会话状态 / 策略 / 输入 / 剪贴板 / 文件"]
    Decoder["原生解码 / 音频输出 / wgpu"]
    Transport["NDP / Noise / QUIC 多路径"]
  end
  React <-->|typed Tauri command| Host
  Host <-->|受控进程管道| Native
  Chrome <--> Native
  Native <--> Session
  Native --> Decoder
  Session <--> Transport
```

React 不直接请求 Manager，不保存 bearer token，也没有任意文件访问或 shell 能力。
它把明确的用户操作交给 Rust，渲染返回的业务数据。登录表单提交密码后清空表单，
不会把密码或票据写到 localStorage、URL 或浏览器日志中。

Rust host 持有身份状态、执行 HTTP 请求、调用原生文件选择器。Session 用独立进程，
不是在 Tauri 的主线程上再启动第二个 winit 事件循环。每个进程拥有自己的原生窗口和
GPU 生命周期；管理页面崩溃或刷新不应重建视频解码器。

两类 IPC 均不传视频帧或音频包。管理页只接收资源、会话状态和传输进度等低频数据。
原生文件路径仅从本机明确操作进入发送流程，不能由远端构造“读取任意本地路径”命令。

服务端和桌面可分别构建。`nebula-desktop` 默认不启用 GUI，避免服务端构建被 Node、
WebKit 或管理页面产物绑住；启用 `gui` 时才引入 Tauri 的窗口和托盘依赖。
打包桌面产品时先构建管理前端和本平台的 Agent/Client，再把原生程序作为 sidecar
放进应用包。开发阶段从工作目录找二进制，不能代替发布包内明确的可执行文件位置。
每个平台必须构建自己的 sidecar，不能把 macOS 二进制带进 Windows 安装包。

桌面产品管理的 Agent 由独立的 `nebula-desktop-agent` helper 运行，它复用
`nebula-agent` 库，不另写采集或传输实现。管理应用退出后，该共享进程可以继续运行；
下次打开应用通过私有、带随机凭据的回环控制端点查询或停止它。
单实例文件锁约束同一身份只运行一个服务，不能靠持久化 PID 后盲目杀进程。
控制端点不承载媒体，也不接受任意命令；凭据与机器身份保存在本机受限文件中，
不交给 WebView。独立部署的 `nebula-agent` CLI 不受这套桌面进程管理接管。

## 3. 服务器与两台电脑的关系

```mermaid
flowchart LR
  subgraph User["用户电脑"]
    UI["管理主窗口"]
    DH["Desktop host"]
    NC["独立 Native Session"]
    UI <-->|本地 IPC| DH
    DH <-->|私有 stdin/stdout| NC
  end
  subgraph Cloud["服务器：可以同机，也可以分开部署"]
    M["Manager<br/>管理 HTTP API"]
    DB[("PostgreSQL")]
    G["Gateway<br/>QUIC UDP 7443"]
    R["Relay<br/>QUIC UDP 7444"]
    M <--> DB
    G <-->|票据公钥 / 状态上报| M
    R -->|节点登记 / 心跳| M
  end
  subgraph Machine["被控电脑"]
    A["Agent 后台进程"]
    OS["系统屏幕 / 音频 / 输入 / 剪贴板 / 下载目录"]
    A <--> OS
  end
  DH <-->|生产 HTTPS| M
  A -->|登记 / 心跳| M
  A <-->|常驻 QUIC 控制连接| G
  NC <-->|QUIC 会话控制| G
  NC <-->|端到端密文| R
  R <-->|端到端密文| A
  NC <-.->|认证后择优的 Direct QUIC| A
```

这里的“QUIC-only”指远程会话的数据传输和 Gateway 信令，不是宣称所有 API 都使用 QUIC。
Manager 当前是 HTTP API；生产部署应提供 HTTPS，开发脚本默认使用本机 HTTP。
没有媒体 TCP 回退。网络禁止到 Gateway/Relay 的 UDP 时，应报告连接失败，而不是换成
另一条未说明的慢速通道。

默认端口只是开发配置。发布地址必须是端点实际可达的地址，不能把服务器内部
`127.0.0.1` 当成给远端设备使用的地址。PostgreSQL 只供 Manager 访问。
生产环境不应沿用开发用的 SSH 反向隧道或测试账号。

### 部署配置与拆分顺序

[`deploy/cloud`](../../deploy/cloud/README.md) 提供三个独立 systemd 服务、统一的
地址配置、独立 HTTPS 入口和证书续期定时器。当前示例将三者放在 `47.103.58.159`：
公网 Manager 为 `https://47.103.58.159`（TCP 443），Gateway 为 UDP 7443，
Relay 为 UDP 7444；内部 Manager 只监听 `127.0.0.1:18081`，数据库不对公网开放。
TCP 80 用于 HTTPS 证书挑战。配置示例不代表某次部署已经启动或完成端到端验收。

Manager 独占组织、用户、用户组、设备和授权数据；Gateway/Relay 不各建一套用户库。
Manager 的内部访问地址与公开票据 issuer 分开配置，后者必须保持为公开 HTTPS 地址。
客户端只配置 Manager，节点通过登记和心跳提供可达的地址与证书 pin。
桌面宿主是本机 API 调用层，不是第二个云端 Manager。

现在同机部署便于运维；后续按地域、带宽优先拆分 Relay，再根据连接规模拆 Gateway。
Manager/数据库按控制面负载独立扩容。服务拆分不自动等于高可用，数据库备份、节点
故障切换、共享配置和运维监控仍需单独建设。

## 4. 从点击“连接”到出现画面

```mermaid
sequenceDiagram
  participant UI as 管理界面
  participant H as Desktop host
  participant M as Manager
  participant C as Native Client
  participant G as Gateway
  participant R as Relay
  participant A as Agent
  A->>G: 预先建立并维持 QUIC 控制连接
  UI->>H: 连接 resource_id
  H->>M: 以用户身份申请会话
  M->>M: 检查租户、账号、资源、授权、在线状态
  M-->>H: 短期 ticket + Gateway 地址与 pin
  H->>C: 启动进程，私有管道传 launch
  C->>G: QUIC 接入并兑换票据
  G->>A: 已授权的 SessionRequest
  G-->>C: Relay 撮合信息
  C->>R: 建立 Relay QUIC 连接
  A->>R: 建立 Relay QUIC 连接
  C->>A: 经 Relay 完成端到端 Noise 握手
  A-->>C: 能力协商、配置与编码帧
  C->>C: 原生解码并呈现第一帧
  C-->>H: 真实状态与测量数据
  H-->>UI: 更新资源的当前会话
  A-->>C: 经认证通道提供直连候选
  C->>A: 校验会话、身份和 pin，探测 Direct
  Note over C,A: 直连可达且更快时切换；否则继续 Relay
```

“已打开窗口”“完成握手”“收到首帧”和“画面已呈现”是不同事件。UI 不应把
成功启动子进程当成已经连接，也不应把队列里的字节数当作播放延迟。

重复点击同一资源返回已有 Session，不并发创建多条连接；连接不同资源可以打开
不同窗口。连接取消、账号退出和子进程退出都必须清理相应的状态，不能留下永久
“正在连接”的卡片。

## 5. Direct 与 Relay 的职责

首先经 Gateway 授权，并在 Relay 上完成端到端握手，之后才尝试直连。
候选地址来自已认证对端，不扫描局域网，也不根据两端公网 IP 一样就推断可以直连。

当前支持可直接到达的 IPv4 和无 scope 的 IPv6 host 地址，没有 STUN 或 UDP 打洞。
Direct 要校验 TLS pin、Noise 身份、会话标识和监听器绑定，再用实际测量选择路径。
Relay 保持低流量热备，不重复转发整份视频。

```mermaid
stateDiagram-v2
  [*] --> Authorizing
  Authorizing --> Relay: 票据与握手成功
  Authorizing --> Ended: 拒绝 / 超时 / 取消
  Relay --> Direct: 候选通过认证且更快
  Direct --> Relay: 直连失效，Relay 可用
  Relay --> Ended: 无可用路径
  Direct --> Ended: 无可用路径
  Relay --> Ended: 端点退出 / 连接关闭
  Direct --> Ended: 端点退出 / 连接关闭
  Ended --> [*]
```

路径切换保留逻辑会话和窗口。可靠通道有有界确认、重发和去重，避免重复注入键鼠
或吞掉文件记录；过时音视频不重放，切换后恢复关键帧与解码参考链。
**Gateway 的会话控制连接仍然保留**，不是 Direct 成功后就把所有服务器断开。
两条路径都不可用时明确结束；重新连接是新的授权申请，不伪装成原连接恢复。

路径可靠性和画面流畅度是两个问题。已有回退机制不等于公网持续帧率和端到端延迟
已达到目标；这部分优化与桌面界面独立推进。

## 6. 用户、设备、资源与权限

```mermaid
erDiagram
  TENANT ||--o{ USER : contains
  TENANT ||--o{ MACHINE : contains
  TENANT ||--o{ USER_GROUP : contains
  TENANT ||--o{ WORKSPACE_INVITATION : issues
  USER ||--o{ GROUP_MEMBERSHIP : joins
  USER_GROUP ||--o{ GROUP_MEMBERSHIP : contains
  USER o|--o{ MACHINE : owns
  MACHINE ||--o{ RESOURCE : publishes
  USER o|--o{ ENTITLEMENT : receives
  USER_GROUP o|--o{ ENTITLEMENT : receives
  RESOURCE ||--o{ ENTITLEMENT : grants
  RESOURCE ||--o{ SESSION : opens
  USER ||--o{ SESSION : initiates
```

机器是资源的宿主，资源才是客户端申请连接的对象。资源可以是桌面，也可以是应用的
发布记录；应用的执行路径属于所有者配置，不必暴露给仅有使用权限的人。
每条授权指向用户或组之一；用户组通过成员关系参与服务端资源权限计算。
删除组会在一个事务中撤销其资源授权并删除成员关系，不删除成员账号。

工作空间对应现有 tenant，类型为 `PERSONAL` 或 `ORGANIZATION`。
自助注册在同一事务中创建新空间、`ADMIN` 所有者和登录凭据，默认关闭，部署者需显式
开启。组织管理员通过 48 小时有效、绑定邮箱、一次性的邀请创建普通 `USER`；
数据库只存邀请码 hash，撤销、到期、重复使用及邮箱不匹配都不能加入。
旧 `OWNER` 角色保留管理员权限，但新账号不自行选择管理员角色。
账号设置仅向组织管理员开放成员和用户组管理，不向普通成员暴露目录枚举。

当前账号归属于一个工作空间，同一邮箱跨空间并非同一个全局账号；
没有多组织 membership 切换、自动邀请邮件、邮箱验证或密码找回。
旧数据升级前必须执行只读邮箱唯一性预检，并暂停旧版本目录写入。
遇到同空间大小写或首尾空白导致的重复邮箱，应人工确认后修正，不能自动合并账号；
具体步骤见云端部署文档。

Manager 根据当前数据库关系判定所有权、授权和会话策略，不信任前端的“我是所有者”
标志。所有操作都在租户范围内执行。拥有某台设备不意味着可以管理整个用户目录；
向指定用户共享时按本租户内的确切邮箱解析，不提供跨租户搜索。

Agent 通过一次性注册令牌取得自己的机器身份，不使用人类的 access token 常驻运行。
桌面「注册本机」显式指定当前用户为设备所有者，并显示所属空间，管理员注册也不例外。
其他空间的既有本机身份不会被登录新账号自动接管；注册账号与开启共享是独立步骤。
用户退出管理账号与撤销设备身份是两件事。移除设备、关闭共享、删除资源、撤销某人的
授权也有不同影响，界面应分别说明并确认，而不是用一个无差别“删除”操作。

当前授权撤销、资源停用和设备移除会阻止新的 Manager 会话申请，但尚无完整的
Manager → Gateway 实时撤销执行链；已经签发的票据和已经建立的连接不会被立即收回。
要立即终止被控端的现有连接，应停止该设备的 Agent 共享服务。
这与停止本应用自己打开的 Client 会话不同，界面不能把修改授权记录宣称为已经踢出对端。

## 7. 窗口与后台服务生命周期

| 操作 | 预期影响 |
| --- | --- |
| 打开管理主窗口 | 进入账号、资源和本机共享界面，不自动申请远程会话 |
| 关闭管理主窗口 | 隐藏管理窗口；不等于停掉本机共享 |
| 关闭 Session | 只结束该连接并释放输入状态、媒体和传输任务 |
| 返回已有 Session | 聚焦原生窗口，不重新登录、不重新申请票据 |
| 退出账号 | 明确提示并关闭该账号的客户端连接，清除人工登录状态 |
| 停止本机共享 | 明确的本机操作，停止受本应用管理的 Agent，不按进程名误杀外部服务 |
| 明确退出桌面应用 | 提示仍在运行的客户端连接，并完成子进程清理 |

Session 管道使用版本化、有界 NDJSON；首条是启动请求，此后是聚焦、断开、声音、
剪贴板和文件发送命令。标准输出专用于协议，诊断写入标准错误。启动票据不能放进
命令行参数。宿主退出导致管道 EOF 时，会话子进程应结束，不能留下无人管理的窗口。

本机共享只能声明实际掌握的状态。macOS 通过无提示的公开 API 查询当前应用的屏幕录制
与辅助功能授权；无法确定的权限仍显示未知，音频不由屏幕授权推断。不能因
Agent 进程启动成功就显示所有权限已开启。系统授权仍由操作系统处理，应用不修改
权限数据库、默认音频设备或网络防火墙来规避权限。

## 8. 媒体、输入和传输

视频保留平台采集/编码、QUIC 传输、原生解码及 wgpu 呈现路径。
视频帧使用独立 QUIC 流，控制与输入有自己的可靠通道和调度优先级；
音频使用 Opus 与 QUIC datagram。界面改动不把原生媒体替换成浏览器录屏或图片轮询。

文字、图片剪贴板和文件的权限独立于画面。macOS 支持 Finder Copy 文件与会话拖入，
接收文件进入 `Downloads/NebulaDesk`；这不是把任意路径透传给远端系统的文件管理器。
完成状态以接收端校验并最终确认成功为准，发送完最后一块不等于成功。
文件名冲突应保留已有内容，传输错误、取消和超时必须释放任务额度。

当前 macOS 键盘按物理键转发，使用远端布局；本地 IME composition/commit 转发尚未完成。
系统音频输出正常与否也不能单凭“采集器启动”判断；实时界面不伪造音频活动或测速值。

## 9. 当前边界与后续扩展

四张设计稿确定布局和主要操作，不把尚未完成的底层能力变成现成承诺：

- 单应用发布元数据和按用户授权，与单应用启动、窗口隔离和串流分别实现；
  后者未完成时，APP 卡片明确不可连接，绝不改连整个桌面。
- 三平台代码构建与实际 GPU、桌面授权、硬件音视频验收不是同一件事。
- OS 自启动服务安装、更新签名和企业分发需要各平台的正式打包方案，
  不能把开发时启动的 Agent 称为已经安装系统服务。
- NAT 打洞、跨网持续视频性能和完整 IME 输入各有独立边界，不借 UI 任务暗中扩大范围。

### 2026-09-09 交付后的未完成项

本轮已通过的 Mac 图形界面、音频、剪贴板和文件结果见
[README 验收记录](../../README.md#packaged-mac-acceptance-2026-09-09)。
本次实际媒体路径是局域网直连，不能将它当作公网中继验收。

| 未完成项 | 当前边界 |
| --- | --- |
| 当前 Mac 包的键鼠验收 | 辅助功能授权仍需用户确认；授权后重连，复测键入、快捷键、点击、拖动、滚轮和失焦释放。 |
| 修正版 App 正式替换 | `7ffafff` 签名开发包已暂存于双方交付目录，尚未替换运行中的 App；需要重新启动、必要时重新登录和授权，再验收修正版。 |
| 单应用连接 | 已有发布记录和按用户/组授权；应用启动、窗口隔离、单应用捕获与串流尚未完成。 |
| 公网与跨 NAT | 持续视频帧率、端到端延迟、拥塞/丢包下恢复仍待优化和验收；现有 Direct/Relay 切换不等于已完成 NAT 打洞。 |
| 授权实时撤销 | 修改权限可阻止新申请，但完整的 Manager → Gateway → 现有会话撤销执行链尚未完成。 |
| 输入法 | 尚无 Client 侧 IME composition/commit 转发，当前使用远端布局的物理键。 |
| Windows/Linux 真机 | 后端与构建已接入，仍缺实际桌面、GPU、系统权限及跨平台互通验收。 |
| 正式安装与分发 | 当前 macOS 使用临时签名开发包；正式签名/公证、安装升级、系统自启动服务和企业分发尚未完整交付。 |
| 账号后续能力 | 自动邀请邮件、邮箱验证、密码找回、同一全局账号跨多个工作空间切换尚未实现。 |

后续增加管理员 Web 控制台时，可复用 Manager API 和部分展示组件，但不应复用桌面
本地特权桥接能力。增加应用串流时，应扩展 Agent 的资源启动与捕获后端以及能力协商，
而不是让前端根据应用名猜测一个远程执行命令。
