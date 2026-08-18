# Nebula — NAT 穿透演进设计 (NAT Traversal Evolution)

> 目的：说明 Nebula 当前传输架构、为什么**不引入 WebRTC**，以及将来需要
> 跨公网/NAT 的 P2P 时如何**以可插拔方式**加入 ICE/STUN/TURN，且**不改动**
> 编解码与渲染管线。

## 1. 两个正交的层

远程串流涉及两层，必须分开理解：

| 层 | 职责 | 现状 |
|----|------|------|
| **NAT 穿透 / 信令** | 让两台不同网络/NAT 后的机器互相发现并打通一条 UDP 路径（STUN 探公网映射、ICE 选路、TURN 中继兜底、信令交换候选） | 占位抽象 `ISignaling`，实现为 `DirectSignaling`（直连，无穿透） |
| **媒体传输** | 在已打通的路径上跑多路复用 + 加密 + 拥塞控制的字节流 | `ITransport`，实现为 QUIC（Network.framework） |

**关键**：这两层正交。换穿透方案**不应该**逼你重写编解码/渲染。

### 1a. 现状更新: relay 作为免费 NAT 观察者 + 轻量打洞（已替代早期 STUN 方案）

早期版本在这里用的是"VDA 向公网 STUN 服务器（`stun.l.google.com`）探测
自己的公网反射地址"。现在已经**移除了这个外部 STUN 依赖**，改为更简单、
延迟更低的方案，原理和 STUN 一致，只是把"观察者"换成了系统本就需要连接
的 `nebula_relay`：

- VDA 和 CWA 本来就要用 QUIC 连 `nebula_relay` 做注册/配对（`RelayHello`）。
  relay 在 msquic 层用 `QUIC_PARAM_CONN_REMOTE_ADDRESS` 直接读出这次连接的
  真实来源 `ip:port`——这就是这台设备的 NAT 映射公网地址，等价于一次免费的
  STUN Binding Response，不需要再问外部服务器。
- relay 在配对成功（`RelayStatus::Paired`）的回复里，**顺带把对端的这个
  观测地址发给另一方**（见 `core/inc/RelayProtocol.h` 的 `PeerAddrWire`），
  双方在收到配对确认的同一时刻就已经知道对方的公网候选地址，不需要额外一次
  往返交换候选。
- **VDA 额外做一次轻量 UDP 打洞**：从自己监听直连的**同一个本地端口**，
  向 CWA 的观测地址发送几个哑负载 UDP 包（`UpgradingTransport.mm` 的
  `FirePunchPackets`），目的不是要建立连接，只是让 VDA 自己的 NAT "看到"
  一次朝 CWA 方向的出站流量，从而放行后续 CWA 主动连过来的入站包——这是
  经典的 UDP 打洞技术（Cohen & Rosenberg, 2005），解决的是 STUN 方案原本
  就解决不了的**端口/地址限制型（restricted-cone）NAT**：那种 NAT 光靠
  "端口映射保持不变"是不够的，必须先有出站流量才会放行对应的入站流量。
- CWA 侧同时也会拿到 VDA 主动广播的**局域网候选地址**（通过 relay 的
  Upgrade 内部通道），并且优先尝试局域网地址，其次才是 relay 观测到的
  公网地址。

这套方案已经写了真实的端到端测试验证（两个独立进程、真实 `nebula_relay`
进程、真实 msquic 连接），确认 relay 能正确读出双方地址、正确嵌入配对
回复、客户端能正确解析并触发候选拨号/打洞包。

这**仍然不是**第 4 节描述的完整 ICE/TURN 方案:
- 对**对称型 NAT**(每个新目的地都换一个外部端口)依然无效——不管地址
  是从 STUN 服务器还是从 relay 观测得到,对称 NAT 下这个地址对第三方
  发起的连接都没有意义,这是 NAT 设备本身的行为限制,任何"协调服务器"
  方案都绕不开。
- 没有真正的 ICE 候选优先级/连通性检查状态机,也没有 TURN——穿透失败时
  的兜底始终是 `nebula_relay` 的盲转发（端到端加密，安全的永久方案，
  不只是权宜之计）。
- Apple `Network.framework` 的 QUIC 不暴露底层的 `PATH_CHALLENGE`/连接
  迁移 API,所以"打洞"在这里的实现是"发哑负载 UDP 包"+"发起新连接尝试",
  而不是复用同一个 QUIC 连接做路径验证再迁移。

也就是说:第 4 节的 libjuice+quiche 方案仍然是"支持对称 NAT 下真正 P2P"
的正确长期路线；这次的改进是在**不引入任何新依赖、不换传输层**的前提下，
用 relay 本来就有的信息把"部分 NAT 类型下的直连成功率"和"首次连接延迟"
都往前推了一步（相比外部 STUN 方案，延迟更低——地址是配对回复的一部分，
没有额外网络往返；覆盖面更广——多了打洞这一步，能应对限制型 NAT）。

## 2. 为什么不引入 WebRTC

WebRTC 是把上面两层**打包**的大栈：ICE + DTLS + SRTP/RTP(媒体) + GCC(拥塞) + 抖动缓冲。
对 Nebula（Mac→Mac、极致低延迟、硬件零拷贝管线）它有三个硬伤：

1. **媒体栈冲突**：WebRTC 的 RTP 打包/抖动缓冲/GCC 是为浏览器互通和高丢包网络
   设计的，会接管帧的时序与缓冲，**夺走**我们对 VideoToolbox→Metal 零拷贝低延迟
   管线的控制权。
2. **重型依赖**：libwebrtc 体积庞大、构建复杂，远超本项目需要。
3. **推翻现有传输**：等于废弃已经工作的 QUIC 实现。

结论：**穿透用更轻的 ICE 库即可，不需要 WebRTC。**

## 3. 当前架构的可插拔点（已就位）

```
        ┌──────────────┐         ┌──────────────┐
        │  VdaServer   │         │  CwaClient   │
        │ (采集/编码)  │         │ (解码/渲染)  │
        └──────┬───────┘         └──────┬───────┘
               │ 只依赖              只依赖 │
               ▼                          ▼
        ┌────────────────────────────────────┐
        │            ISignaling              │  ← 换穿透方案只动这两个接口的实现
        │  Direct (现) | Ice (未来)           │
        ├────────────────────────────────────┤
        │            ITransport              │
        │  Quic直连 (现) | IceQuic (未来)     │
        └────────────────────────────────────┘
```

- `ITransport`（`common/inc/Transport.h`）：通道收发语义。`CreateTransport(type)`
  工厂；`TransportType::IceQuic` 已预留枚举，目前回退到直连 QUIC。
- `ISignaling`（`common/inc/Signaling.h`）：`start(role, onRemote)` 解析对端，
  `PeerDescriptor` 含 `host/port` 和预留的 `blob`（将来装 ICE 候选/SDP）。
- `CwaClient::connect` 已经走 **signaling → transport** 两步，所以未来替换
  实现时上层逻辑零改动。

## 4. 将来加 NAT 穿透：推荐路线（路线 B）

> 仅在确有"跨公网 P2P"需求时再做。

**重要约束**：Apple Network.framework 的 QUIC **不允许自带外部 socket**，
因此无法直接消费 ICE 协商出来的 UDP 路径。一旦要做 ICE，传输层需换成
**可自带 socket 的 QUIC**。

### 组件
| 组件 | 选型 | 作用 |
|------|------|------|
| ICE/穿透 | **libjuice**（纯 C, ISC 协议, 轻量） | STUN 绑定探公网地址、ICE 打洞、TURN 中继兜底 |
| QUIC(自带socket) | **quiche**(Cloudflare) 或 **ngtcp2** 或 **msquic** | 在 libjuice 打通的 UDP 路径上跑 QUIC |
| STUN/TURN 服务器 | **coturn** | 公网反射地址 + 中继 |
| 信令服务器 | 轻量 WebSocket 服务 | 交换 ICE 候选 / 传输参数（offer/answer） |

### 数据流（未来）
```
1. 两端各自:  libjuice gather → 本地/反射(STUN)/中继(TURN) 候选
2. 信令服务器:  交换候选 (走 ISignaling::blob)
3. libjuice:  连通性检查 → 选出最优 UDP 路径
4. 在该 socket 上启动 quiche QUIC → 复用现有三通道协议
5. VideoToolbox / Metal 管线完全不变
```

### 落地步骤
1. 新增 `IceSignaling implements ISignaling`：接入信令服务器 + libjuice 候选收集，
   把对端候选填入 `PeerDescriptor::blob`。
2. 新增 `IceQuicTransport implements ITransport`：用 libjuice 的 socket 喂给
   quiche，复用现有 `NebulaProtocol` 三通道帧格式。
3. `CreateTransport(TransportType::IceQuic)` / 信令工厂里接上即可，
   `VdaServer`/`CwaClient`/编解码/渲染**零改动**。
4. 部署 coturn + 信令服务，配置 STUN/TURN 地址。

## 5. 决策摘要

- ✅ 现在：保持 QUIC 直连，已把传输/信令抽象解耦，留好口子。
- ✅ 将来跨公网：libjuice(ICE) + quiche(QUIC)，作为 `ISignaling`/`ITransport`
  的新实现插入。
- ❌ 不采用 WebRTC：媒体栈冲突、依赖重、伤性能。
