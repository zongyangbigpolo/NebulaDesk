# Nebula — 总体架构与路线图 (Architecture & Roadmap)

> 本文档汇总所有已锁定的架构决策，作为 P0/P1/P2 实施依据。
> 配套文档：`ARCHITECTURE.md`(当前实现)、`NAT_TRAVERSAL.md`(穿透演进)、`PLAN.md`。

## 0. 已锁定的设计决策

| # | 决策 | 选择 |
|---|------|------|
| D1 | 视频默认编码 | **HEVC 硬件编码**(AV1 在 Apple Silicon 无硬件编码,不适合低延迟) |
| D2 | 最低系统 | **macOS 26**(其他平台将来) |
| D3 | 传输协议 | **QUIC** 为主(原生 Nebula 客户端);**WebRTC(ICE/DTLS/SRTP,经 libdatachannel)** 为浏览器互通的第二条路径,详见 §12 |
| D4 | NAT 穿透库 | **libjuice(经 libdatachannel 接入)**(ICE/STUN/TURN),已用于 WebRTC 路径;原生 QUIC 直连路径的候选发现是自研的 relay 观测地址 + 轻量 UDP 打洞(见 §4) |
| D5 | 管理器 UI | **Flutter**(跨平台),不渲染媒体 |
| D6 | session 窗口 | **独立原生进程 + 高刷渲染**(mac=Metal),不复用 Flutter |
| D7 | 中继模式 | **模式 A 盲转发 + 应用层端到端加密**(ChaCha20-Poly1305,见 ARCHITECTURE.md §4a;中继现在只经手密文) |
| D8 | 中继实现 | **纯 C++ + msquic**(跨平台,与客户端协议一致);可选接入外部 SaaS 控制面做授权回调 |
| D9 | 打包 | **单一 .app**(Flutter 为壳,内嵌 session helper) |
| D10 | 连接策略 | **中继兜底 + 自动升级直连**(relay→direct upgrade,直连掉线自动回退中继并重试) |
| D11 | 实施顺序 | **P0 → P1 → P2 → P3** |
| D12 | SaaS 控制面(可选) | **Node.js + TypeScript + PostgreSQL**(`server/nebula_cloud/`),账号/设备归属/授权分享/票据签发,通过 HTTP 回调授权中继配对,不改变中继盲转发的信任模型 |

## 1. 目标架构总览

```
┌──────────────── Flutter 管理器 (.app 主进程, 跨平台) ─────────────┐
│  设备列表 / 添加VDA / 中继配置(自定义relay_url) / 连接 / 活跃会话  │
│  不碰媒体栈                                                        │
└───────────────┬──────────────────────────────────────────────────┘
                │ Process.start() + IPC状态通道(契约)
                ▼
┌──────────────── session 进程 (每会话一个, 原生高刷) ──────────────┐
│  core(C++) + platform后端: 连接→解码→Metal高刷渲染→输入采集        │
└───────────────┬──────────────────────────────────────────────────┘
                │ ITransport (Direct / Relay / 将来Ice)
                ▼
   ┌────────── Relay Server (nebula_relay, 用户可自建) ──────────┐
   │  盲转发 + 在线设备表 + 配对 + 候选交换(升级用)              │
   └──────┬──────────────────────────────────────┬─────────────┘
          ▼ (升级成功后旁路)                       ▼
       VDA(被控) ←──────── 直连 QUIC (优先) ────────→ CWA(查看)
```

## 2. 可移植边界 (Core vs Platform Backend)

**Core(纯C++,全平台共用)** 与 **Backend(每平台一份)** 严格分层。
编排层(VdaServer/SessionClient)只依赖 Core 接口,全平台一份代码。

| 能力 | Core 接口 | mac 后端(现有) | win(将来) | linux(将来) |
|------|-----------|---------------|-----------|-------------|
| 传输 | `ITransport` | Network.framework QUIC | msquic | msquic/quiche |
| 信令 | `ISignaling` | Direct | Direct | Direct |
| 抓屏 | `IScreenCapture` | ScreenCaptureKit | DXGI Dup | PipeWire |
| 视频编码 | `IVideoEncoder` | VideoToolbox | MF/NVENC | VAAPI |
| 视频解码 | `IVideoDecoder` | VideoToolbox | MF/D3D | VAAPI |
| 音频编解码 | `IAudioCodec` | AudioToolbox | MF | FFmpeg |
| 渲染 | `IRenderer` | Metal | D3D11 | Vulkan |
| 音频播放 | `IAudioPlayer` | AVAudioEngine | WASAPI | PipeWire |
| 输入注入 | `IInputInjector` | CGEvent | SendInput | uinput |
| 输入捕获 | `IInputCapture` | NSView | Win32 | X11/Wayland |
| 进程拉起 | `IProcessSpawner` | posix_spawn | CreateProcess | posix_spawn |

### 接口必须平台中立(P0 要修的债)
现有能力接口在签名里泄漏了 Apple 类型(`CVPixelBufferRef`/`CMSampleBufferRef`/
`CVImageBufferRef`),必须改成平台中立的帧描述(裸数据+宽高+格式+时间戳),
把 Apple 类型锁进 mac 后端内部。

### 互通约束(跨平台 CWA↔VDA)
- **视频码流格式钉死 Annex-B** + 参数集走 config 帧(VideoToolbox 输出 AVCC,
  mac 后端负责转 Annex-B;Windows MF 原生 Annex-B)。否则跨平台解不了。
- **传输互通**:统一 ALPN="nebula"、TLS1.3、证书校验策略;长期全平台统一 msquic。

## 3. 中继服务器 (nebula_relay)

### 模式 A 盲转发 + 应用层端到端加密 [已实现]
- 中继做字节转发,但转发的字节本身是 ChaCha20-Poly1305 密文
  (见 ARCHITECTURE.md §4a)。中继只掌握配对元数据(device-id/token/ticket),
  看不到 HELLO 协商、鼠标键盘事件或音视频内容,可以把中继视为不可信的纯管道。

### 穿内网原理
- VDA 和 CWA **都主动出站**连中继(NAT 默认放行出站),无需公网IP/端口映射 →
  天然穿透,成功率高于打洞。

### 会话票据与配对 [已实现]
```
1. VDA 上线 → 连中继 → 注册 device-id(用户预设或分配),此后长期保持注册,
   可反复被连接,不会因为配对过一次就失效
2. CWA 连接 → 出示 {目标device-id + 配对token(或上次会话的reconnect ticket)}
3. 中继校验(本地token/ticket比对,或委托给可选的外部SaaS控制面)
   → 对接两条QUIC连接成一个会话管道;若该VDA已有查看端,旧查看端收到
   Superseded 状态后被断开,新查看端顶替
4. 管道内跑 NebulaProtocol(载荷已加密);配对成功后中继下发一个新的
   短期 reconnect ticket 给 CWA,存本地缓存,下次连接优先使用
```

### 二次重连 [已实现,含一处明确的取舍]
- **VDA 重连**:同一 device-id 重新注册即可,中继维护的注册表本身就是长期
  存在的,不会因为配对而移除;若 VDA 掉线时还有查看端连着,中继会把这个
  查看端保持连接 30 秒宽限期,VDA 在窗口内重新注册就自动接回原会话,
  查看端全程无感、不需要重新握手。
- **CWA 重连**:客户端缓存了配对成功后中继下发的 reconnect ticket
  (`~/Library/Application Support/Nebula/relay_tickets.txt`,按 device-id
  索引),下次连接优先出示 ticket,10 分钟有效期内免去重新出示长期 token。
- **心跳**:中继侧对每条 QUIC 连接开启 `KeepAliveIntervalMs=15000` 的 PING
  心跳,早于 60 秒空闲超时探测到死连接。
- **"一对一"限制的取舍(诚实记录)**:一个 VDA 同一时刻只服务**一个**查看端
  (新查看端接入会顶替旧的),不是多个查看端同时并发观看同一路串流。
  真正的并发多观看端需要 VDA 侧对每个查看端单独完成 KeyInit/KeyInitAck
  握手并分别加密同一帧画面(因为每个会话的加密密钥必须独立、不可共享,
  否则会出现同密钥同 nonce 的加密安全问题),这需要 VdaServer 支持多个并行
  transport/pipeline,属于比"中继按 device-id 路由"大得多的架构改动,本轮
  未实现,留作后续工作。

## 4. 连接升级 (Relay → Direct Upgrade) 【核心,已实现】

### 流程
```
T0 双方连中继 → 立即可用(经中继),画面先出
T1 双方在 relay 配对回复中免费拿到彼此的观测公网地址(见 NAT_TRAVERSAL.md §1a);
   VDA 另广播局域网候选,并向 CWA 的观测地址发几个哑负载 UDP 打洞包
T2 CWA依次尝试候选(先局域网,约1秒无响应再试relay观测到的公网候选)+ 连通性探测
T3 直连探测成功 → 媒体流迁移到直连
T4 直连稳定 → 旁路中继转发(中继只留配对信息)
   失败/直连断 → 自动回退中继,永不断流,并在数秒后重新广播候选、重试升级
```

### NAT 穿透的真实覆盖范围(诚实记录)
候选交换目前包含两类地址:
1. **局域网地址**(`LocalIPv4()`)——双方在同一网络时几乎总能成功,这是最常见场景。
2. **relay 观测到的公网地址**(见 `core/inc/RelayProtocol.h` 的 `PeerAddrWire` +
   `server/nebula_relay/main.cpp` 的 `GetRemoteAddr`)——relay 直接从双方连它的
   QUIC 连接上读出真实来源地址,随配对回复免费下发给对方,不再需要问外部 STUN
   服务器(已移除 `StunClient.mm`)。对 cone / 端口保留型 NAT(常见的家用/办公
   路由器)有效。VDA 一侧还会额外向 CWA 的观测地址发送几个哑负载 UDP 包
   (`UpgradingTransport.mm` 的 `FirePunchPackets`,经典 UDP 打洞技术),让 VDA
   自己的 NAT 提前"看到"出站流量,从而也能覆盖端口/地址限制型(restricted-cone)
   NAT——这是相对纯 STUN 方案新增的覆盖面。

**这不是完整的 ICE/TURN 实现**:对**对称型 NAT**(每个新目的地都换一个外部
端口)完全无效,那种情况会一直停留在中继上——这是已知、如实记录的限制,
不是 bug,任何"协调服务器"方案都绕不开这一点。完整方案(D4:libjuice
ICE/STUN/TURN)仍待实现,见 NAT_TRAVERSAL.md。

### 为什么"新建直连+迁移"而非 QUIC 原生迁移
QUIC 原生 migration 只换同一连接的路径,无法把"经中继的连接"变成"直连对端"
(不同 socket)。正解:应用层 **会话ID不变**,底层新建直连QUIC,验证OK后改走直连。
会话状态(编解码器/序列号/密钥)与传输连接**解耦** —— `NebulaProtocol` 帧头已含
seq+timestamp,解码器迁移时无缝接上;加密会话密钥独立于传输连接派生一次,
迁移路径也不需要重新握手。

### 抽象:升级是 ITransport 内部的事,上层无感
```
UpgradingTransport
  ├─ 启动: 经中继收发,立即可用
  ├─ 后台: relay免费下发对端观测地址 + LAN候选广播 + VDA打洞包 + 直连探测(逐个候选尝试,带超时)
  ├─ 升级: 直连就绪 → 切发送路径到 DirectTransport
  └─ 回退: 直连失败/断 → 切回中继,重置状态允许下次重新升级
```
VdaServer/CwaClient 只见"一个 ITransport 在收发",升级/回退透明。

### ConnectionPolicy 优先级
```
1. 有直连地址(局域网/已知公网) → DirectTransport
2. 否则 → RelayTransport 立即可用 → 后台升级直连(LAN优先,relay观测公网候选次之) → 成功切换
3. 直连不可达(对称NAT) → 保持中继(用户无感,多一跳),断线也会自动回退到此状态
```

## 5. Flutter 管理器 ↔ session 契约

- Flutter 用 `Process.start()` 拉起原生 `nebula_session` 子进程。
- **启动参数**:目标(device-id 或 host:port)、中继配置、会话票据。
  敏感数据(token/密钥)走 **环境变量或 stdin 管道**,不走命令行(防 ps 泄露)。
- **IPC 状态通道**:session 回报 连接中/已连接/直连升级成功/断开/错误/帧率;
  管理器据此显示活跃会话、支持远程终止。
- session 关窗即退出,Flutter 据退出码刷新列表。
- **Flutter 不渲染媒体**:解码帧塞进 Flutter texture 会引入额外拷贝+合成延迟,
  故 session 独立进程直接驱动 CAMetalLayer,绕开 Flutter。

### 高刷渲染
- `CAMetalLayer.maximumDrawableCount=3` + `CVDisplayLink` 跟随刷新率
  (ProMotion 120Hz 自适应),独占主循环跑满高刷。

## 6. 目标目录结构

```
core/                 纯C++全平台: 协议/传输接口/信令接口/加密/ConnectionPolicy/编排
platform/
  mac/                现有.mm后端搬入: VideoToolbox/Metal/SCK/CGEvent/Network
  win/  linux/        将来(空)
app/
  manager/            Flutter 跨平台UI
  session/            原生高刷 CWA 查看器
server/
  nebula_relay/       中继(纯C++ + msquic, 可独立部署/用户自建;可选接入下面的SaaS做授权)
  nebula_cloud/       可选 SaaS 控制面(Node.js/TS + PostgreSQL): 账号/设备/授权分享/
                      票据签发/Web控制台,不是媒体路径的一部分
```

## 7. 实施路线

| 阶段 | 内容 | 产出 | 状态 |
|------|------|------|------|
| **P0**(地基) | 接口去Apple类型;目录 core/platform/app;协议钉死Annex-B;新增 IRenderer/IAudioPlayer/IInputCapture/IProcessSpawner 占位 | 可移植边界 | ✅ 完成 |
| **P1**(UI/多进程) | 拆 app/session(原生高刷)+ Flutter app/manager + spawn/IPC契约 + 单.app打包 | 你的UI诉求 | ✅ 完成 |
| **P2**(中继) | nebula_relay盲转发(msquic) + RelaySignaling/RelayTransport + 会话票据 + 重连 + **relay→direct升级** | 你的中继诉求 | ✅ 完成 |
| **P2.5**(安全与可靠性) | 应用层端到端加密(ChaCha20-Poly1305) + 直连断线自动回退 + VDA重连宽限期 + CWA重连ticket + 中继心跳 + relay观测地址+轻量UDP打洞 + 可选SaaS授权控制面 | 消除阻断级风险 | ✅ 完成(NAT穿透仅覆盖cone型/限制型NAT,并发多观看端仅做顺序顶替,详见§3/§4) |
| **P3**(跨平台,将来) | win/linux后端;完整ICE/TURN直连(含对称NAT);全平台统一msquic;VDA支持并发多观看端(需多路独立加密会话) | 跨平台落地 | ⬜ 待做 |
| **P4**(浏览器互通) | 见 §12:libdatachannel 接入真实 WebRTC(ICE/DTLS/SRTP),nebula_cloud 加信令,自研 Web 客户端,coturn 部署 | 任意浏览器/WebRTC客户端直接观看 | 🚧 进行中 |

> 仅 P0/P1/P2/P2.5 在 macOS 上实现;Win/Linux 代码 P3 再写,但 P0 的边界保证将来只增不改。
> **relay→direct 升级已端到端验证**:VDA 注册 → CWA 配对(加密握手)→ relay 盲转发桥接 →
> 候选交换(LAN+relay观测地址)→ 直连探测 → 媒体迁移到直连 → relay 兜底,断线自动回退。

## 12. P4 — 浏览器互通:引入 WebRTC(推翻 D3)【进行中】

### 为什么推翻原来"不引入 WebRTC"的决策

D3/D4 最初拒绝 WebRTC,理由是"引入完整 libwebrtc 太重、会夺走零拷贝管线控制权"。
但用户明确要求"浏览器能直接打开看画面"这个能力——**这是真实浏览器唯一认的协议**,
没有绕过 ICE/DTLS/SRTP 的办法。原来的顾虑针对的是 **Google 的 libwebrtc**(体积庞大、
构建复杂、接管媒体时序),不是 WebRTC 协议本身。

### 选型:libdatachannel,不是 libwebrtc,也不抄 crossdesk 的 GPL/LGPL 代码

- **[libdatachannel](https://github.com/paullouisageneau/libdatachannel)**:C++ 实现的
  WebRTC Data Channels + Media Transport + WebSocket,**MPL 2.0** 协议(弱著佐权,
  按文件生效,允许与其他协议的代码组合分发,不要求整个项目跟着开源)。
  作者同时维护 **libjuice**(ICE,MPL 2.0)。
- 系统依赖全部走 Homebrew/发行版包管理器,不 vendor 别人的 GPL/LGPL 代码:
  `libjuice`(ICE,MPL-2.0)、`srtp`(SRTP,BSD-3-Clause)、`libusrsctp`(SCTP 数据通道,
  BSD-3-Clause)、`opus`(音频编码,BSD-3-Clause)、系统 OpenSSL(DTLS 后端)。
- **Web 客户端和信令服务器都是自研的**,只把 crossdesk-web-client 的交互设计当参考,
  不复制其代码,避免其 LGPL v3 协议的传染性。

### 架构

```
浏览器 (自研 Web 客户端, RTCPeerConnection)
   │ WebSocket 信令 (SDP offer/answer + ICE候选, JSON)
   ▼
nebula_cloud (Fastify 加 WS 端点,复用现有账号鉴权,纯转发不碰媒体)
   │ WebSocket (VDA 主动出站长连接注册)
   ▼
nebula_vda (新增 WebRtcSession: rtc::PeerConnection)
   ├─ Video track: 复用现有 VideoEncoder(H264 Annex-B)→ rtc::H264RtpPacketizer
   ├─ Audio track: 新增 OpusEncoder(libopus)→ RTP(Opus)
   └─ Data channel: 复用现有 NebulaInputEvent 编解码,鼠标键盘反控
ICE 连通性: STUN(内网/部分NAT直连)+ TURN(coturn,对称NAT兜底,标准开源软件)
```

这条路径与现有的 QUIC 原生客户端路径(`nebula_session`/`nebula_relay`)**完全独立**,
互不影响——原生客户端不需要 WebRTC 栈,浏览器客户端不需要 msquic,VDA 同时支持
两种查看端接入。

### 已知代价(如实记录)

- DTLS/SRTP 是安全关键代码,即使用了成熟库也需要认真验证配置正确性。
- 浏览器 WebRTC 标准音频是 Opus,不是我们原生路径用的 AAC——两套编码器并存。
- 需要独立部署一个 TURN 服务器(coturn)才能覆盖对称 NAT,这是标准基础设施,
  文档化部署而非自己实现。
- 目前仍是并发单观看端的限制(与 §3 的顺序顶替一致),多观看端广播是后续工作。
