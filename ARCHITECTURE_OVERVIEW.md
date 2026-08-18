# Nebula — 项目架构总览 (Architecture Overview)

> 本文档描述**当前这个文件夹里实际实现了什么**：完整的系统架构、各模块职责、
> 数据/控制流、以及模块间的依赖关系。用于快速理解整个项目现状，不是设计
> 意向书——凡是标注"已实现"的部分都有对应代码和测试支撑。
>
> 配套文档：[README.md](./README.md)（快速上手）、[USAGE.md](./USAGE.md)（运行指南）、
> [ARCHITECTURE.md](./ARCHITECTURE.md)（线路协议细节）、
> [ROADMAP.md](./ROADMAP.md)（架构决策记录）、[NAT_TRAVERSAL.md](./NAT_TRAVERSAL.md)（穿透方案）、
> [server/nebula_cloud/README.md](./server/nebula_cloud/README.md)（SaaS 服务 API）。

## 1. 一句话概括

**Nebula 是一个 macOS 远程桌面系统**：一台 Mac（VDA，被控端）把屏幕和声音采集、
编码后传给另一台设备（CWA 原生客户端，或直接用浏览器）查看和反向操控。
支持局域网直连、跨网络中继穿透、断线自动重连，媒体全程应用层加密；
另外还有一套可选的 SaaS 账号系统，用于管理设备归属、分享授权和浏览器直连。

## 2. 系统全景图

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│                              被控端 Mac（VDA / nebula_vda）                       │
│                                                                                   │
│  ScreenCaptureKit          VideoToolbox            AudioToolbox                  │
│  (虚拟显示器采集) ──► HEVC/H264 硬件编码 ──┐   系统音频 ──► AAC 编码 ──┐          │
│         │                                  │                          │          │
│         │                                  ▼                          ▼          │
│         │                          ChaCha20-Poly1305 应用层加密（NebulaCrypto）   │
│         │                                  │                          │          │
│         │                    ┌─────────────┴──────────┐               │          │
│         │                    ▼                        ▼               ▼          │
│         │              QUIC 直连/中继通道         WebRTC 通道(可选) ◄─ Opus 编码  │
│         │              (Control/Video/Audio)     (H264/Opus/DataChannel)         │
│         │                    │                        │                          │
│  CGEvent 输入注入 ◄───────────┴────────────────────────┘                          │
│         ▲                                                                        │
└─────────┼────────────────────────────────────────────────────────────────────────┘
          │ 鼠标键盘反控（NebulaInputEvent，24字节定长格式，两条通道共用同一格式）
          │
┌─────────┴───────────────────────────────┐     ┌────────────────────────────────┐
│   QUIC 路径：直连 / 经中继升级直连         │     │   WebRTC 路径：浏览器直接观看    │
│                                          │     │                                 │
│  nebula_session（原生高刷查看器）          │     │  浏览器 RTCPeerConnection       │
│  Metal 渲染 + AVAudioEngine 播放          │     │  <video> 播放 + DataChannel 输入 │
│         ▲                                │     │         ▲                       │
│         │ QUIC(直连/中继)                 │     │         │ ICE/DTLS/SRTP        │
│         │                                │     │         │                       │
│  ┌──────┴────────┐                       │     │  ┌──────┴──────────┐            │
│  │ nebula_relay  │  盲转发+重连宽限期+     │     │  │ nebula_cloud    │            │
│  │ (msquic, C++) │  reconnect ticket+     │     │  │ /ws/signaling   │            │
│  │ 可选SaaS授权   │  QUIC心跳               │     │  │ (WS 信令转发)    │            │
│  └───────────────┘                       │     │  └────────┬────────┘            │
└──────────────────────────────────────────┘     └───────────┼────────────────────┘
                                                              │
                                              ┌───────────────┴────────────────┐
                                              │  nebula_cloud (Node/TS+Postgres)│
                                              │  账号 / 设备归属 / 分享授权 /    │
                                              │  短期会话票据 / Web 控制台       │
                                              └─────────────────────────────────┘

                     ┌───────────────────────────┐
                     │  app/manager (Flutter)     │  管理 VDA 列表、一键拉起
                     │  桌面图形界面（不碰媒体）    │  nebula_session 子进程
                     └───────────────────────────┘
```

## 3. 目录结构与职责

```
Nebula/
├── core/                纯 C++，全平台可移植，不含 Apple 类型
│   ├── inc/             接口头文件（协议、加密、传输、编解码接口、WebRTC封装）
│   └── src/             VdaServer/CwaClient 编排逻辑 + 加密 + WebRTC 桥接
├── platform/mac/        macOS 专属后端实现（.mm 文件）
│   ├── inc/
│   └── src/             ScreenCaptureKit/VideoToolbox/Metal/QUIC 等实现
├── app/
│   ├── vda/              被控端可执行文件入口（nebula_vda）
│   ├── session/           查看端可执行文件入口（nebula_session）
│   └── manager/           Flutter 图形管理界面
├── server/
│   ├── nebula_relay/      QUIC 中继服务器（纯 C++ + msquic）
│   └── nebula_cloud/      SaaS 控制面（Node.js + TypeScript + PostgreSQL）
├── tests/                 原生 C++ 单元测试
├── scripts/               构建/打包脚本
└── *.md                   架构文档
```

## 4. 核心能力矩阵

| 能力域 | 实现位置 | 状态 |
|---|---|---|
| 屏幕采集（虚拟显示器，强制） | [platform/mac/src/ScreenCapture.mm](platform/mac/src/ScreenCapture.mm), [VirtualDisplay.mm](platform/mac/src/VirtualDisplay.mm) | ✅ |
| 视频硬编解码（HEVC/H264） | [platform/mac/src/VideoEncoder.mm](platform/mac/src/VideoEncoder.mm), [VideoDecoder.mm](platform/mac/src/VideoDecoder.mm) | ✅ |
| 音频（系统声音，AAC） | [platform/mac/src/AudioEncoder.mm](platform/mac/src/AudioEncoder.mm), [AudioDecoder.mm](platform/mac/src/AudioDecoder.mm) | ✅ |
| Metal 零拷贝渲染 | [platform/mac/src/MetalRenderer.mm](platform/mac/src/MetalRenderer.mm) | ✅ |
| 鼠标键盘反控 | [core/inc/NebulaInput.h](core/inc/NebulaInput.h), [platform/mac/src/InputInjector.mm](platform/mac/src/InputInjector.mm) | ✅ |
| QUIC 直连传输 | [platform/mac/src/QuicTransport.mm](platform/mac/src/QuicTransport.mm) | ✅ |
| 应用层端到端加密 | [core/inc/NebulaCrypto.h](core/inc/NebulaCrypto.h)（ChaCha20-Poly1305） | ✅ |
| 中继穿透 + 断线自动升级直连 | [platform/mac/src/RelayTransport.mm](platform/mac/src/RelayTransport.mm), [UpgradingTransport.mm](platform/mac/src/UpgradingTransport.mm) | ✅ |
| relay 观测地址 + 轻量 UDP 打洞 | [server/nebula_relay/main.cpp](server/nebula_relay/main.cpp)（`GetRemoteAddr`）, [UpgradingTransport.mm](platform/mac/src/UpgradingTransport.mm)（`FirePunchPackets`） | ✅（对称NAT无效，见NAT_TRAVERSAL.md） |
| 中继服务器（VDA重连宽限期/ticket/心跳） | [server/nebula_relay/main.cpp](server/nebula_relay/main.cpp) | ✅ |
| 中继可选接入 SaaS 授权回调 | `--saas-auth-url`（同上文件） | ✅ |
| WebRTC 浏览器观看（H264/Opus/DataChannel） | [core/inc/WebRtcSession.h](core/inc/WebRtcSession.h), [WebRtcGateway.h](core/inc/WebRtcGateway.h) | ✅ |
| Opus 音频编码（WebRTC 专用） | [core/inc/OpusAudioEncoder.h](core/inc/OpusAudioEncoder.h) | ✅ |
| SaaS 账号/设备/授权/票据 | [server/nebula_cloud/src](server/nebula_cloud/src) | ✅ |
| WebRTC 信令转发（WS） | [server/nebula_cloud/src/signaling.ts](server/nebula_cloud/src/signaling.ts) | ✅ |
| 浏览器 Web 客户端 | [server/nebula_cloud/public/watch.js](server/nebula_cloud/public/watch.js) | ✅ |
| Flutter 图形管理器 | [app/manager/lib](app/manager/lib) | ✅ |
| 跨平台（Windows/Linux）后端 | — | ⬜ 未实现 |
| 完整 ICE/TURN（对称NAT，原生QUIC路径） | — | ⬜ 未实现（WebRTC路径已支持TURN） |
| 并发多观看端（QUIC路径） | — | ⬜ 顺序顶替，非同时并发 |

## 5. 三种连接方式详解

### 5.1 局域网直连（最简单）

```
nebula_vda --port 7000
                │ QUIC (Control / Video / Audio 三条独立连接)
                ▼
nebula_session --host <VDA-IP> --port 7000
```

- 三个逻辑通道各自独立 QUIC 连接，互不阻塞。
- 首条消息是明文 `KeyInit`/`KeyInitAck` 握手（交换随机 salt），随后所有
  `Hello/HelloAck/Video/Audio/Input` 都用派生出的会话密钥 ChaCha20-Poly1305
  加密，`seq` 复用做重放保护。

### 5.2 经中继穿透 + 自动升级直连

```
VDA ──(注册 device-id)──► nebula_relay ◄──(device-id+token配对)── CWA
       盲转发（密文）加密媒体流，画面先出

              后台并行：relay配对回复免费下发对端观测地址 + 交换局域网候选
                       + VDA向CWA观测地址发UDP打洞包
                             │
                     CWA 逐个尝试直连候选(LAN优先)
                             │
                    探测成功 → 媒体流切到直连
                    （中继仍保留作为兜底）
                             │
                  直连中途掉线 → 自动回退中继，
                  重置状态，后台重新尝试升级
```

关键可靠性设计（[server/nebula_relay/main.cpp](server/nebula_relay/main.cpp)）：

- VDA 长期注册在中继上，可反复被连接，无需每次重启。
- VDA 掉线时给已连接的 CWA 保留 30 秒宽限期，VDA 重新注册即可无感恢复。
- 配对成功后下发一个 reconnect ticket，CWA 本地缓存
  （`~/Library/Application Support/Nebula/relay_tickets.txt`），
  下次连接优先用 ticket，免去重复出示长期共享密钥。
- QUIC KeepAlive 心跳（15s），早于 60s 空闲超时探测死连接。
- 新查看端接入会顶替旧查看端（`Superseded` 状态），**同一时刻仍只服务一个
  QUIC 查看端**——这是当前明确记录的限制，不是并发广播。

### 5.3 浏览器直接观看（WebRTC，全新能力）

```
nebula_vda --webrtc --webrtc-signaling-url ws://<cloud>/ws/signaling \
           --device <id> --token <relay-token> --stun ... --turn ...
                │
                │ WebSocket 信令（仅转发 SDP/ICE，JSON小消息）
                ▼
        nebula_cloud /ws/signaling
                │
                ▼
     浏览器 RTCPeerConnection（原生 Web API）
     ICE/DTLS/SRTP 直接与 VDA 建立媒体连接
     <video> 播放 + DataChannel 回传鼠标键盘
```

- 基于 [libdatachannel](https://github.com/paullouisageneau/libdatachannel)
  （MPL 2.0），不是 Google 的 libwebrtc，也没有使用任何 GPL/LGPL 代码。
- 视频走 H264（浏览器 WebRTC 广泛支持的唯一编码），音频走 Opus（新增的
  独立编码器，与原生路径的 AAC 并存）。
- **这条路径天然支持多个浏览器同时并发观看**——每个连接独立完成 DTLS/SRTP
  握手，没有 QUIC 路径那种共享加密密钥的限制。
- 鼠标键盘通过 DataChannel 传输，编码格式与原生 CWA 完全一致
  （同一个 24 字节 `NebulaInputEvent` 结构体）。
- STUN 之外还支持标准 TURN（`--turn`），可以覆盖对称型 NAT
  ——这一点原生 QUIC 直连升级路径目前做不到（原生路径靠 relay 观测地址 +
  轻量 UDP 打洞，见 NAT_TRAVERSAL.md，对对称NAT无效）。

## 6. 线路协议（NebulaProtocol）

所有消息共享 24 字节小端定长头（`NebulaFrameHeader`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| magic | u32 | `kNebulaMagic` = `0x5542454E`（"NEBU"） |
| version | u8 | 协议版本（当前 2） |
| type | u8 | 消息类型 |
| flags | u16 | bit0=关键帧, bit1=编解码配置数据 |
| length | u32 | 载荷长度（加密后为密文长度） |
| seq | u32 | 每通道每发送方递增序号，同时是 AEAD nonce 的输入 |
| timestampUs | u64 | 采集/播放时间戳 |

消息类型：`HELLO=1, HELLO_ACK=2, VIDEO=3, AUDIO=4, INPUT=5, BYE=6, KEY_INIT=7, KEY_INIT_ACK=8`。

**加密**：除 `KEY_INIT`/`KEY_INIT_ACK` 外全部载荷用 ChaCha20-Poly1305 加密
（[core/inc/NebulaCrypto.h](core/inc/NebulaCrypto.h)），会话密钥由共享密钥
（PSK）+ 双方随机 16 字节 salt 经 HKDF-SHA256 派生；nonce = 通道号 + 发送方
角色 + 序号，帧头本身作为 AEAD 关联数据防止跨消息重放。这使得 `nebula_relay`
的"盲转发"真正成立——中继只经手密文，从协议层面看不到画面、声音或输入内容。

## 7. 数据模型（SaaS 层）

`server/nebula_cloud` 用 PostgreSQL + Prisma，核心表：

```
User ──owns──► Device ──has──► AccessGrant ──references──► User(grantee)
                  │
                  └──has──► ConnectionAudit（每次签发/使用会话票据的审计记录）
```

- `Device.relayDeviceId` / `relayTokenHash`：VDA 注册到 `nebula_relay` 用的
  长期凭证（哈希存储，明文只在创建时返回一次）。
- `AccessGrant.role`：`VIEWER` / `CONTROLLER`，所有者身份是隐式的 `OWNER`。
- `ConnectionAudit`：每次 `/connect` 签发的短期 JWT 都会落一条审计记录，
  `nebula_relay` 或 `nebula_cloud` 自己的信令端点会在真正建立连接前**再查一次
  数据库**确认授权没有被撤销（而不是只信任 JWT 本身），这是"实时鉴权回调"
  设计的核心目的。

## 8. 关键设计取舍（如实记录）

| 决策 | 选择 | 原因 |
|---|---|---|
| 传输协议 | QUIC（原生路径）+ WebRTC（浏览器路径），二选一按场景 | QUIC 给原生客户端最优性能；WebRTC 是浏览器唯一选项 |
| 视频编码 | 原生路径默认 HEVC，WebRTC 路径强制 H264 | HEVC 硬件编码效率更高；但浏览器 WebRTC 广泛支持的只有 H264 |
| 加密 | 应用层 ChaCha20-Poly1305，独立于传输层 TLS | 让中继的"盲转发"承诺在协议层面真正成立，不依赖 TLS 信任链 |
| 中继模型 | 一对一顺序顶替，非广播 | 多播需要 VDA 侧支持多路独立加密会话，是更大的架构改动 |
| NAT 穿透 | 原生路径是 relay 观测地址+轻量UDP打洞；WebRTC 路径支持完整 TURN | libdatachannel 已内置 TURN 支持；原生 QUIC 路径的完整 ICE/TURN 需要更换传输层（见 NAT_TRAVERSAL.md） |
| SaaS 与媒体路径解耦 | `nebula_cloud` 只签发票据和转发信令，不碰媒体字节 | 保持中继"看不到内容"的安全属性，SaaS 层出问题不影响媒体机密性 |

## 9. 测试覆盖

| 测试 | 位置 | 验证内容 |
|---|---|---|
| 输入协议编解码 | `tests/test_input.cpp` | `NebulaInputEvent` 24 字节格式往返 |
| 会话加密 | `tests/test_crypto.cpp` | 加解密、篡改检测、重放拒绝、乱序拒绝 |
| WebRTC offer 生成 | `tests/test_webrtc_session.cpp` | 真实 ICE gathering + SDP 内容校验 |
| SaaS API | `server/nebula_cloud/tests/api.test.ts` | 注册/登录/设备/授权/票据签发/鉴权回调 |
| WebRTC 信令路由 | `server/nebula_cloud/tests/signaling.test.ts` | 真实 WebSocket 客户端走完整信令流程 + 两种拒绝场景 |

全部原生测试通过 `ctest` 运行，全部 SaaS 测试通过 `npm test`（vitest）运行；
另外做过一次真实的端到端联调：真实编译的 `nebula_vda --webrtc` 进程连到真实
跑起来的 `nebula_cloud`，模拟浏览器走完整信令握手并收到合法 SDP offer。

## 10. 尚未实现 / 明确排除的范围

- **跨平台后端**（Windows/Linux）：接口已抽象（`ITransport`/`IScreenCapture`
  等），具体实现是空的。
- **原生 QUIC 路径的完整 ICE/TURN**：目前只有局域网候选 + relay 免费下发的
  对端观测公网地址 + 轻量 UDP 打洞（见 NAT_TRAVERSAL.md §1a），对称型 NAT
  下仍会一直停留在中继上。
- **QUIC 路径并发多观看端**：目前是顶替而非广播。
- **TURN 服务器本身**：文档化了如何接 `coturn`，但仓库内不包含 TURN 服务器
  实现。
- **SaaS 生产加固**：当前本地开发实例用随机生成密钥、无 HTTPS，未做生产部署
  加固（真实部署需要参考 `server/nebula_cloud/README.md` 里的环境变量表自行
  配置）。
