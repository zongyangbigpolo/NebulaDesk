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

**授权按会话边界生效，是已确认的产品行为，不是待补的实时撤权功能。**
Manager 在申请新会话时检查最新授权，已建立的连接沿用其权限快照；撤销资源授权不会
主动踢出已有会话。Direct/Relay 路径切换不算新会话，结束后再次连接则必须重新申请。
已经签发但尚未使用的票据仍按原短有效期和单次兑换规则处理，不增加即时吊销。
资源停用和设备移除也会阻止新的 Manager 会话申请。要立即终止被控端现有连接，
应明确停止该设备的 Agent 共享服务，不能把修改授权记录宣称为已经踢出对端。

## 7. 窗口与后台服务生命周期

| 操作 | 预期影响 |
| --- | --- |
| 打开管理主窗口 | 进入账号、资源和本机共享界面，不自动申请远程会话 |
| 关闭管理主窗口 | 隐藏管理窗口；不等于停掉本机共享 |
| 断开 Session / 关闭桌面会话窗口 | 只结束该连接并释放输入状态、媒体和传输任务，不请求退出远端应用 |
| 关闭应用窗口 | 发送带 Surface 与几何版本的正常关窗请求；等待远端销毁或保存对话框，不能先结束整个连接 |
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
| 输入法 | 尚无 Client 侧 IME composition/commit 转发，当前使用远端布局的物理键。 |
| Windows/Linux 真机 | 后端与构建已接入，仍缺实际桌面、GPU、系统权限及跨平台互通验收。 |
| 正式安装与分发 | 当前 macOS 使用临时签名开发包；正式签名/公证、安装升级、系统自启动服务和企业分发尚未完整交付。 |
| 账号后续能力 | 自动邀请邮件、邮箱验证、密码找回、同一全局账号跨多个工作空间切换尚未实现。 |

后续增加管理员 Web 控制台时，可复用 Manager API 和部分展示组件，但不应复用桌面
本地特权桥接能力。增加应用串流时，应扩展 Agent 的资源启动与捕获后端以及能力协商，
而不是让前端根据应用名猜测一个远程执行命令。

## 10. 单应用连接的跨平台设计建议

以下是设计方向，不代表已经实现或完成三平台验收。先用两台 Mac 验证公共契约及 macOS
后端，Windows/Linux 必须保留独立后端和真实能力检查，不能用编译成功冒充功能可用。

### 10.1 复用边界

保留 Manager 的资源与授权、Gateway 撮合、Relay 密文转发及 Client 原生解码渲染。
现有 `media.rs` 已用 `Platform`、`VideoSource`、`AudioSource`、`InputInjector` 分离平台；
需要补的是会话目标、应用生命周期、窗口集合与输入作用域，而非另写一套传输协议。

| 概念 / 拟议接口 | 职责 |
| --- | --- |
| `SessionTarget` | 区分桌面与应用资源；目标绑定已授权资源及其发布版本，不接收 Client 提供的任意命令行。 |
| `ApplicationHost` | 平台适配层解析受信任发布配置并启动/绑定应用，返回 `ApplicationInstance`；路径和参数分别处理，不拼接 shell 命令。 |
| `ApplicationInstance` | 持有本次启动的实例身份、所有权和生命周期；不能仅凭 PID 或应用名认领用户已经打开的其他窗口。 |
| `SurfaceRegistry` | 追踪获准主窗口、子窗口、尺寸、DPI、层叠与销毁；用稳定的会话内 `SurfaceId` 和几何版本屏蔽原生句柄。 |
| `PlatformSession` | 从现有 `Platform::session_scope` 演进，持有本会话的捕获、输入、音频和 OS 授权资源。 |
| `AppSession` | 编排启动、等待窗口、媒体建立、窗口变更和清理；复用当前会话媒体管线。 |
| 能力协商 | 分别报告窗口捕获、窗口输入、子窗口、应用音频和隔离等级；按资源实际需求决定是否允许连接。 |

发布配置或目标版本在票据签发后发生变化时，不得静默执行另一份程序；Agent 必须校验
授权目标和解析出的启动计划一致。断连默认释放会话资源，不强杀用户原有进程；
是否关闭本次独占启动的应用应由明确的生命周期策略控制。

### 10.2 三平台后端

| 平台 | 原生方向 | 必须显式处理的差异 |
| --- | --- | --- |
| macOS | LaunchServices / NSWorkspace；ScreenCaptureKit 窗口捕获；Accessibility 与 CoreGraphics 输入 | 应用复用旧进程、多个文档窗口、弹窗归属、焦点和坐标变化；窗口捕获不等于安全沙箱。 |
| Windows | Win32 启动与窗口跟踪；Windows.Graphics.Capture 的 `CreateForWindow`；窗口/输入 API | 多进程应用、owned windows、DPI、UIPI 与 UAC；不能假定 `SendInput` 只影响目标窗口。 |
| Linux Wayland | 应用启动；ScreenCast Portal 的窗口选择、PipeWire；RemoteDesktop Portal / libei | 标准 Portal 不保证按任意 PID 无人值守选窗或应用级输入隔离；依赖 compositor 的能力，不能照搬 Win32。 |

Linux 首期需要明确支持的 compositor/portal 组合；在不满足目标绑定和控制要求的环境
中返回不支持或明确需要本机选择，不得偷偷改为全屏共享。无人值守和独立用户桌面可通过
后续专用 compositor / 隔离桌面后端扩展，但不是标准窗口 Portal 自动提供的能力。

### 10.3 呈现、输入和隐私

2026-09-10 已确认最终效果：每个获准远端窗口映射为一个 Client 原生窗口，参与本地
移动、缩放、最小化和任务切换，应用内容仍保持远端平台风格。单个容器只能用于内部
验证，不能代替最终 seamless 交付。未经确认归属的窗口、菜单和系统弹窗不能自动加入。

输入携带目标 Surface 和几何版本，在 Agent 校验后才转换坐标；窗口失效、归属不明、
版本过期或焦点无法确认时停止投递并反馈状态，不能落到桌面。音频应按应用/进程作用域
捕获，不支持时明确禁用，不能退回整机系统声音。应用模式也不能直接沿用整机全局
剪贴板自动同步，应默认关闭，后续以明确作用域和用户同意接入。

**单应用呈现与多用户安全隔离是不同能力。** 共享同一个 OS 登录会话时，即使只采集一个
窗口，也无法由此保证文件访问、系统对话框、其他应用启动和并发输入的安全隔离。
首期共享桌面模式应限制为受信任用户，并对同一 OS 交互桌面的控制会话采用独占租约；
面向不可信用户的应用发布必须依赖独立 OS 会话、沙箱或虚拟机等另行设计的隔离环境。

### 10.4 推荐落地顺序

1. 定义目标、实例、Surface、能力协商及失败状态；桌面路径保持不变，APP 默认仍禁用。
2. 在 Mac 上实现一个受控应用的启动、窗口绑定、捕获和基础输入，验证缩放与关闭流程。
3. 验证多窗口/弹窗、最小化、焦点竞争、应用退出、断连清理，以及不捕获无关窗口。
4. 按已验证公共契约实现 Windows/Linux 后端；仅对满足能力和隔离要求的资源启用 APP。
5. 单独推进应用音频、受限剪贴板和更强隔离，不把桌面模式的权限直接搬进应用模式。

## 11. Seamless 实施计划（2026-09-10）

用户已批准开始实现。此处记录实施范围与验收门槛，不能把计划项当作已经完成。
按 NebulaDesk 自有客户端实现，不接入 Citrix Workspace app 的 ICA/HDX 协议。

| 阶段 | 实施内容 | 完成门槛 |
| --- | --- | --- |
| 公共契约 | 应用目标绑定、能力协商、Surface 生命周期、输入及窗口操作 | 不支持 APP 的旧客户端被拒绝；桌面序列化与行为兼容；消息有尺寸、数量和作用域限制。 |
| 控制面与管理入口 | 可信应用配置随授权送到 Agent；用户点击应用资源直接连接 | Client 不传可执行命令；发布配置不被误当成运行能力；不要求消费者选择机器；无桌面回退。 |
| Agent 应用会话 | 独占控制租约、启动实例、窗口跟踪、按窗口捕获与输入 | 只处理获准实例窗口；不认领无关旧窗口；拒绝过期坐标；不泄漏整机音频和剪贴板。 |
| Client 原生窗口 | 每个 Surface 独立窗口/解码器，父子关系、焦点和窗口操作 | 主窗口和弹窗可独立呈现；关闭发起远端正常关闭，保存确认尚未完成时不销毁整个会话。 |
| 跨平台键盘 | 原始物理键与快捷键适配分离，按应用选择映射 | Windows 到 Mac 编辑快捷键可映射 Command；终端 Control-C 不误变成复制；释放和失焦无粘键。 |
| 平台后端 | Mac 真机优先，Windows/Wayland 使用相同契约 | 后端未实现或缺能力时明确拒绝；Linux 选窗/输入受 Portal 实际能力约束；不伪报三平台验收。 |
| 集成交付 | 相关回归、本地成品构建、两台 Mac 原生窗口联调 | 逐项记录已验证与未验证的能力；签名更新不绕过 OS 权限；用户登录和设备身份保留。 |

重点场景包括：启动失败、没有窗口、应用复用旧实例、文档和保存对话框、缩放/DPI 变化、
快速创建/关闭窗口、视频先于窗口元数据到达、窗口销毁后迟到的视频、断线和正常关闭。
Surface 标识不能在同一会话内随意复用；媒体与控制跨流到达不能把图像或输入投递给
另一个窗口。macOS 全局应用菜单需要单独处理，不能假定窗口捕获已经包含菜单栏。

已有资源授权变更继续只影响后续新会话申请；本轮不增加实时踢出。

### 11.1 已落地契约与当前边界

- 可信 `LaunchTarget::Application` 将资源 ID、发布配置版本与 `ApplicationLaunch` 绑定到签名
  票据。Gateway 原样转发票据；Agent 必须使用已注册 Manager 的可信授权元数据验签。
  Manager 的连接地址不一定等于 JWT issuer，不能从反向代理 URL 猜测 issuer。
- 资源读取、会话申请及信令两端均显式协商 `application_windows`。桌面宿主只接收非敏感
  模式提示，校验资源种类与提示一致后通过 `Launch.application_windows` 启动子进程；
  不向 Client 传递独立的可执行路径或参数配置，也不在失败时回退桌面。
- APP 视频负载为 `VideoFrameInfo`、8 字节 `SurfaceFrameInfo`、编码数据。后者同时携带
  `geometry_generation` 与 `surface_sequence`；每个 Surface 独立解码和排序，不使用
  multipath 重写后的全通道序号推断单窗口丢帧。
  首期每个会话最多分配 32 个不同 Surface，已移除的 ID 不复用；这是整个会话的累计
  额度，不只是同时打开的窗口数，保存对话框也占用额度。无限窗口生命周期尚未实现。
- APP 的整机声音、剪贴板和文件通道关闭。资源 DTO、票据、宿主文件选择入口与子进程
  命令分别检查有效权限；已有会话继续使用启动时快照，不从刷新后的资源授权重新赋权。
- 管理界面提供与关窗不同的“断开”操作。应用详情可选择物理键、编辑或终端模式；
  默认物理键，选择仅保留在当前登录期间、下次新连接生效。此偏好只成为固定的本地
  Client 参数，不发给 Manager 作为应用启动配置。IME composition/commit 仍未实现。
- macOS 全局应用菜单尚未实现，不采集桌面菜单栏来冒充支持。Windows 当前有进程/
  窗口归属、WGC 与常规控件输入基础，但完整作用域键盘/IME/Raw Input 尚不满足契约；
  Linux 有需本机同意的 Window-only Portal/PipeWire 基础，但无法证明与签名应用目标
  的对应关系。两者均保持 `IsolationUnavailable`，不能称三平台 seamless 已可用。

构建、协议回归和模拟窗口测试不等于真机应用验收。需另行确认新签名应用的录屏与
辅助功能权限，并用独立测试应用验收多窗口捕获、保存取消、输入与断连；不得修改
TCC 数据库、复用无关应用身份或绕过系统授权。开发临时签名的代码哈希变化可能使
旧版本授权不再适用，正式签名/公证仍是独立交付项。

### 11.2 本机原生联调记录（2026-09-10，阶段性）

当前 Mac mini 的实际系统为 macOS 27.0。使用独立测试账号、设备身份和数据库，
运行真实 Manager、Gateway、Relay、原生 Agent 与 Client；连接包含本机网络 Direct
候选，不是两台 Mac 或公网性能验收。夹具显式以 macOS 26 为最低部署目标编译。

已在完整原生 Client 路径观察到：

- 两份文档对应两个独立窗口和正确的不同画面。ScreenCaptureKit 产生的系统录屏控件
  不再被当作第三个应用窗口转发；过滤依据是窗口内容归属，不是标题或尺寸黑名单。
- 从 Client 正常关闭第一份文档时，仅相应源窗口和 Client 窗口消失，第二份继续更新。
  关闭最后一份干净文档正常结束连接。夹具画面中的计数器不是视频帧率。
- 未保存文档的关闭请求保留会话并显示独立保存对话框；修正后对话框不再包含缩小的
  整个父窗口，父视频也不重复合成该对话框。源端执行 Cancel 后，Client 对话框消失，
  未保存父文档保留。这项结果验证生命周期传播，不冒充 Client 鼠标输入验收。

后续完整网络回归已通过原生 A/B/C 与 Escape 按键、真实 Reset 按钮动作、缩放后的新尺寸
硬解码、关闭一窗后的另一窗持续出图，以及最后 Remove/Bye。鼠标使用公开 NSEvent API
绑定窗口和转换坐标，不保留私有 SPI 或全局输入回退。第二个窗口启动不再阻塞首窗发帧，
经重新确认的同尺寸窗口不因短暂 AX 不可用而无谓重启采集。

最终复核补充了生产端帧版本标记，贯穿原生图像缓存、编码和排队，旧帧不能在恢复后
改标为新版本；静态窗口恢复后没有新关键帧时，有时限地重启该窗口采集，不拖停其他窗口。
真实保存对话框回归确认服务端拒绝发给被阻塞父窗口的 Enter/Escape，而发给对话框的
Escape 能正常取消；明确选择 Discard 后正常关闭，不需要再次点击关闭。
这些边界均有对应回归，复核已关闭发现的问题。同机 Client 与被控应用竞争 OS
焦点，不能以源端 Accessibility 按钮操作替代跨机输入结果。另一台测试 Mac 在 19:32
短暂恢复 SSH 后再次不可达，尚未部署或验收最新包；当前调试进程已有权限也不代表
新签名 App 自动继承权限。

仓库提供可复现的 macOS 原生夹具；其文字和按键记录仅属于测试文档，不读取用户应用。
以下命令需要已授权的空闲图形桌面及独立测试 PostgreSQL 数据库，不能使用生产数据库，
也不能与另一个远程控制会话并行运行：

```sh
bash scripts/build-seamless-fixture.sh
export NEBULA_APP_PROBE_PATH="$PWD/target/seamless-fixture/NebulaSeamlessFixture.app"
export NEBULA_APP_PROBE_STATUS_DIR="$PWD/target/seamless-fixture/status"
export NEBULA_TEST_DATABASE_URL="postgres://localhost/nebula_seamless_test"
cargo test -p nebula-agent --test live \
  native_app_two_documents_resize_and_close_normally -- --ignored --exact --nocapture
```

该回归建立真实控制面和加密媒体路径，检查独立硬解码、原生按键/按钮效果、缩放与正常
关闭；不会自动授予系统权限。每次运行通过新启动进程的 PID 匹配状态文件，不使用可能被
其他夹具覆盖的 latest 文件。失败后只清理本次测试应用，不退出其他已打开的用户应用。
原生 Client 的窗口呈现与两机互通仍需另外验收。
