# nebula_relay — 部署指南 (Deployment)

`nebula_relay` 是一个**平台中立**的 QUIC 中继(纯 C++ + msquic + libcurl,无任何 Apple 依赖),
用于在 VDA 与 CWA 无法直连时盲转发媒体流(载荷本身是 ChaCha20-Poly1305 密文,
中继看不到画面/输入内容,见 [ARCHITECTURE.md](../../ARCHITECTURE.md) §4a),
并帮助两端交换候选地址以**升级为直连**(局域网 + 尽力而为的 STUN 公网候选)。
生产环境推荐部署在 **Linux VPS / 云服务器**。

> 协议:QUIC over **UDP**。务必放行 UDP 端口(默认 7100),TCP 无效。

---

## 方式一:Docker(最简单,推荐)

```sh
# 在仓库根目录构建镜像(Dockerfile 会自动装 libmsquic 并编译)
docker build -t nebula-relay -f server/nebula_relay/Dockerfile .

# 运行(注意 /udp!)
docker run -d --name nebula-relay --restart unless-stopped \
    -p 7100:7100/udp nebula-relay

# 查看日志
docker logs -f nebula-relay
```

镜像内置自签证书。生产建议挂载你自己的证书:
```sh
docker run -d --name nebula-relay --restart unless-stopped \
    -p 7100:7100/udp \
    -v /etc/nebula-relay:/etc/nebula-relay:ro \
    nebula-relay \
    --port 7100 --cert /etc/nebula-relay/relay_cert.pem --key /etc/nebula-relay/relay_key.pem
```

---

## 方式二:systemd(裸机/VM,一键脚本)

在服务器上(Debian/Ubuntu),拿到仓库后:
```sh
sudo ./server/nebula_relay/install_linux.sh --port 7100
```
脚本会自动:装 libmsquic + 编译工具链 → 编译 relay → 装到 `/usr/local/bin` →
生成自签证书到 `/etc/nebula-relay` → 建 `nebula-relay` 系统用户 + systemd 服务 →
开机自启 + 放行 ufw 防火墙。

管理:
```sh
systemctl status nebula-relay
journalctl -u nebula-relay -f
systemctl restart nebula-relay
```

---

## 方式三:手动编译(任意 Linux 发行版)

### 1) 安装 libmsquic
- **Ubuntu/Debian**(微软官方源):
  ```sh
  curl -sSL https://packages.microsoft.com/keys/microsoft.asc \
    | sudo gpg --dearmor -o /usr/share/keyrings/microsoft.gpg
  . /etc/os-release
  echo "deb [signed-by=/usr/share/keyrings/microsoft.gpg] https://packages.microsoft.com/ubuntu/${VERSION_ID}/prod ${VERSION_CODENAME} main" \
    | sudo tee /etc/apt/sources.list.d/microsoft.list
  sudo apt-get update && sudo apt-get install -y libmsquic libcurl4-openssl-dev
  ```
- **其他发行版**:从源码构建 msquic(见
  https://github.com/microsoft/msquic/blob/main/docs/BUILD.md),并安装
  发行版自带的 libcurl 开发包(`libcurl4-openssl-dev` / `libcurl-devel` 等)。

### 2) 编译 relay(独立,无需整个仓库)
只需三个文件:`server/nebula_relay/main.cpp`、`CMakeLists.standalone.txt`、
`core/inc/RelayProtocol.h`。
```sh
cd server/nebula_relay
cp CMakeLists.standalone.txt CMakeLists.txt   # 独立构建用这个
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
```

### 3) 证书
```sh
sudo mkdir -p /etc/nebula-relay
sudo openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout /etc/nebula-relay/relay_key.pem \
  -out    /etc/nebula-relay/relay_cert.pem \
  -days 3650 -subj "/CN=your-relay-domain"
```

### 4) 运行
```sh
./build/nebula_relay --port 7100 \
  --cert /etc/nebula-relay/relay_cert.pem \
  --key  /etc/nebula-relay/relay_key.pem
```

---

## 防火墙 / 云安全组(关键!)

中继是 **UDP**,必须在两处放行:
1. **主机防火墙**:`sudo ufw allow 7100/udp`
2. **云厂商安全组**(AWS/阿里云/腾讯云等):入站规则放行 **UDP 7100**
   —— 这一步最常被遗漏,导致客户端连不上。

验证端口在监听:
```sh
ss -ulpn | grep 7100      # 应看到 nebula_relay 绑定 UDP 7100
```

---

## 客户端如何使用这个中继

被控端(VDA):
```sh
nebula_vda --port 7000 \
  --relay <中继公网IP或域名> --relay-port 7100 \
  --device mymac --token <配对密钥>
```
查看端(CWA / session):
```sh
NEBULA_PSK=<共享密钥> nebula_session \
  --relay <中继公网IP或域名> --relay-port 7100 \
  --device mymac --token <配对密钥>
```
或在 Flutter 管理器里给该 VDA 填入 Relay URL。

`device` 是设备标识(VDA 注册、CWA 用它找 VDA),`token` 是配对令牌(两端必须一致)。
CWA 侧会在配对成功后自动缓存中继下发的 reconnect ticket,下次连接优先使用,
无需每次都重新出示 `token`(见 [../../USAGE.md](../../USAGE.md))。

---

## 可选:接入 SaaS 控制面做授权(server/nebula_cloud)

如果不想让中继单纯靠一个静态 `token` 做配对判断,而是要账号体系 + 可撤销的
授权分享(参照 [crossdesk](https://github.com/kunkundi/crossdesk) 的模式),
可以部署 `server/nebula_cloud/`(Node.js/TS + PostgreSQL,独立服务,详见其
[README](../nebula_cloud/README.md)),再给中继加两个参数:

```sh
./nebula_relay --port 7100 \
  --cert /etc/nebula-relay/relay_cert.pem --key /etc/nebula-relay/relay_key.pem \
  --saas-auth-url  https://<nebula_cloud_host>/internal/authorize \
  --saas-auth-secret <与 nebula_cloud 的 RELAY_SHARED_SECRET 一致>
```

开启后,CWA 的配对请求不再跟 VDA 注册时的静态 token 做比较,而是实时回调
`nebula_cloud` 的 `/internal/authorize`(带 `X-Relay-Secret` 头),由它校验
短期会话 JWT 的签名/有效期,并且**每次都查一次数据库确认授权没有被撤销**。
VDA 的注册流程不受影响,仍然用 `nebula_cloud` 创建设备时分配的长期 token。

---

## 生产加固清单

- [ ] 用**真实证书**(Let's Encrypt 等)替换自签证书;并去掉客户端的 trust-all
      校验(`RelayTransport.mm` 里的 verify block)。这一步只影响传输层 TLS,
      不影响应用层的端到端加密(§4a),但仍然建议做,防止中继身份被冒充。
- [ ] 生产环境建议接入上面的 **SaaS 授权回调**(`--saas-auth-url`),而不是只
      依赖静态 token——静态 token 一旦泄露就要手动改 VDA 注册的 token,
      而 SaaS 模式下撤销授权立即生效,且有完整的连接审计。
- [ ] 多实例 + 负载均衡时,注意会话亲和(同一 device 的 VDA/CWA 要落到同一实例,
      因为配对表、reconnect ticket、VDA重连宽限期都在进程内存中)。
- [ ] 监控:`journalctl` / 容器日志;关注 `paired` / `Superseded` / `VDA reattached` /
      `shutdown` 事件。
- [ ] 带宽:兜底转发期间中继承担全部媒体流量,按并发会话数 × 码率估算。
      升级直连成功后中继流量回落;注意 STUN 候选对**对称型 NAT** 无效,那部分
      会话会长期停留在中继上(见 [NAT_TRAVERSAL.md](../../NAT_TRAVERSAL.md))。

---

## 资源占用与扩展

- 单会话在**升级直连前**经中继转发,中继上行≈下行≈会话码率(默认 ~20 Mbps)。
- 升级直连成功后,中继仅保留极小的信令/心跳流量。
- 因此中继带宽成本主要取决于「**打洞失败、长期走中继**」的会话比例。
