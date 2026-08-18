# Nebula — 计划书 (Plan)

## 1. 项目目标

构建一个**仅支持 macOS** 的远程显示框架，模仿 [icagraphics](https://) 的分层架构，
实现 **Mac VDA（被控端/服务器）→ Mac CWA（控制端/查看器）** 的：

- **第一阶段**：实时**视频画面 + 音频**串流（VDA 采集 → 编码 → QUIC → CWA 解码 → 渲染/播放）。
- **第二阶段**：**鼠标 / 键盘**反向操控（CWA → VDA）。

核心诉求：**极致性能**。为此采用全硬件编解码、零拷贝纹理、QUIC 多流避免音视频互相阻塞。

## 2. 关键技术选型

| 领域 | 选型 | 理由 |
|------|------|------|
| 语言 | C++17 + Objective-C++ (`.mm`) | 与 icagraphics 一致，直接调用 Apple 框架 |
| 传输 | **Apple Network.framework QUIC** (`NWProtocolQUIC`) | 最原生、系统级优化、内建 TLS1.3、多 stream |
| 屏幕采集 (VDA) | **ScreenCaptureKit** (`SCStream`) | macOS 现代高性能采集，支持系统音频 |
| 视频编码 (VDA) | **VideoToolbox** 硬件编码 (HEVC 默认 / H.264) | GPU 硬编，低延迟 |
| 视频解码 (CWA) | **VideoToolbox** 硬件解码 | 输出 `CVPixelBuffer`，零拷贝到 Metal |
| 音频采集 (VDA) | ScreenCaptureKit 系统音频 / Core Audio | 与画面同源 |
| 音频编码 | **AudioToolbox AAC**（低延迟）| 原生，Opus 可后续替换 |
| 画面渲染 (CWA) | **Metal** (`CAMetalLayer`) + `CVMetalTextureCache` | 零拷贝 YUV→RGB |
| 音频播放 (CWA) | **AVAudioEngine** / Core Audio | 低延迟播放 |
| 构建 | **CMake**（Ninja/Xcode） | 与 icagraphics 一致 |

## 3. 模块划分（对应 icagraphics 的 VDA/CWA 分层）

```
Nebula/
├── common/   两端共享：协议、QUIC 传输封装、日志、类型
├── vda/      服务端：采集 → 编码 → QUIC 发送
├── cwa/      客户端：QUIC 接收 → 解码 → 渲染/播放 + 窗口
└── tests/
```

详见 [ARCHITECTURE.md](./ARCHITECTURE.md)。

## 4. 数据流

```
VDA: SCStream 采集 ─► VideoToolbox 编码 ─┐
                                          ├─► QUIC(独立 stream) ─► 网络
     系统音频 ─► AAC 编码 ───────────────┘
CWA: 网络 ─► QUIC ─► 解复用 ─► VideoToolbox 解码 ─► Metal 渲染
                              └► AAC 解码 ─► AVAudioEngine 播放
```

## 5. 线路协议（运行于 QUIC 之上）

- **Control stream**（双向）：握手 `HELLO`、能力协商；第二阶段承载输入事件。
- **Video stream**（单向 VDA→CWA）：分帧的编码视频。
- **Audio stream**（单向 VDA→CWA）：分帧的编码音频。

每个消息带 16 字节帧头：magic / version / type / flags / length / timestampUs / seq。
使用独立 stream 让音视频互不发生 head-of-line blocking。

## 6. 实施阶段

1. **文档**：本计划书 + 架构文档。✅
2. **common**：协议帧定义、`QuicTransport` 封装、日志、类型。
3. **vda**：`ScreenCapture` → `VideoEncoder` / `AudioEncoder` → `VdaServer` → `main`。
4. **cwa**：`CwaClient` → `VideoDecoder` / `AudioDecoder` → `MetalRenderer` / `AudioPlayer` → 窗口。
5. **构建**：两端 CMake target，验证 configure/build。
6. **第二阶段（后续）**：control stream 上的鼠标/键盘事件。

## 7. 性能要点

- 全程硬件编解码，避免 CPU 软编。
- `CVPixelBuffer` → `CVMetalTexture` 零拷贝渲染。
- 音视频拆分到不同 QUIC stream，避免互相阻塞。
- QUIC `NWParameters` 启用 1-RTT、禁用 Nagle 类延迟（datagram/低延迟选项）。
- 编码器配置：实时模式、低延迟、关闭 B 帧、周期性关键帧。
