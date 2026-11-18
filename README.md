# ESP32-C3 + HC-SR501 红外感应 → Wi-Fi 上报模板

阻塞式主循环，不用 Embassy / 异步：

```
等红外变高 → 上报一次 → 等红外变低 → 继续等
```

上报有三套，切换只要改 `src/net.rs` 里的 `TRANSPORT` 这一行，
默认是 UDP。Wi-Fi 射频 blob 的后台由 `esp-rtos` 驱动，主循环不用管它。

## 接线

| HC-SR501 | ESP32-C3 开发板 |
|---|---|
| VCC | **5V 脚**（USB 进来的那路，不要接 3.3V，也不要用 GPIO 供电） |
| GND | GND（和 C3 共地） |
| OUT | **GPIO10** |

GPIO10 的选择：避开了 strapping 脚（GPIO2/8/9）、Flash 占用的 GPIO11~17、
USB-Serial-JTAG 的 GPIO18/19、UART0 的 GPIO20/21。

## 结构

- `src/main.rs`：主程序，只有红外逻辑，约 100 行
- `src/net.rs`：联网的脏活全在这里（Wi-Fi 连接、DHCP、UDP/TCP 上报）
- `tools/test_server.py`：接收端，UDP 和 TCP 两种模式，先验证上报通不通

## 需要改的配置

Wi-Fi 和服务器地址都在 `src/net.rs` 顶部：

- `TRANSPORT`：上报走哪条路，`Transport::Udp`、`Transport::Tcp`
  或 `Transport::Ntfy`，改这一行就行，默认 UDP
- `SSID` / `PASSWORD`：编译时用环境变量传入，不要写死在代码里提交
- `SERVER_IP` / `SERVER_PORT`：接收端服务器的地址（只影响 UDP 和 TCP 版）
  - 模板不用 DNS，**直接写 IP**。接收端可以是局域网里的一台电脑、
    家里的服务器，或者一个 UDP/HTTP 接收服务（先用固定 IP 或内网 IP）
  - UDP 版发的是一条数据报，内容为 `{"event":"motion","count":N,"uptime_ms":M}`，
    `count` 是每次触发 +1 的全局序号，接收端靠它去重
  - TCP 版发的是 `POST /motion`，body 为 `{"event":"motion","count":N}`
- `NTFY_TOKEN`（只影响 ntfy 版）：编译时用环境变量传入，
  不要写死在代码里提交。ntfy 服务器地址写在 `src/net.rs` 顶部的
  `NTFY_HOST`（域名）和 `NTFY_PATH`（`/motion`，topic 叫 `motion`），
  一般不用改。ntfy 版第一次上报前 DNS 解析一次服务器 IP 并缓存
  （用 DHCP 给的 DNS，没给就用 8.8.8.8），之后每次上报直接用缓存的
  IP，不再解析；只有推送在连接层失败时才清掉缓存、下次上报重新解析
  （应对 Cloudflare 换 IP），DNS 的时间成本每次上电只付一次。
  再走 TLS 1.3 POST，请求头带 `Authorization: Bearer <NTFY_TOKEN>` 和
  `Title: 有人经过`，body 是纯文本 `Motion #N detected`。根证书是 `assets/isrg-root-x1.der`
  （Let's Encrypt 的 ISRG Root X1，2035 年到期），证书链和主机名都会校验。
- `BURST_COUNT` / `BURST_INTERVAL_MS`（只影响 UDP 版）：一次触发连发几次、
  间隔多久，默认 10 次、间隔 500 ms

## 环境准备

```bash
rustup toolchain install stable
rustup target add riscv32imc-unknown-none-elf
cargo install espflash --locked
```

## 编译、烧录、看日志

```bash
SSID=你家WiFi名 PASSWORD=你家WiFi密码 cargo run --release
```

用 ntfy 版（`TRANSPORT` 改成 `Transport::Ntfy`）时，还要传 token：

```bash
SSID=你家WiFi名 PASSWORD=你家WiFi密码 NTFY_TOKEN=你的ntfy-token cargo run --release
```

`cargo run` 会自动烧录并打开串口监视。板子进奇怪状态时先擦除再烧：

```bash
espflash erase-flash
```

## 先验证上报通不通

`tools/test_server.py` 是一个零依赖的接收端，UDP 和 TCP 两种模式，
`TRANSPORT` 改成哪个就用对应模式跑它：

```bash
python3 test_server.py udp        # 监听 UDP 8080，带去重和收包统计
python3 test_server.py tcp        # 监听 TCP 8080，接 HTTP POST
python3 test_server.py udp 9000   # 换个端口
```

把 `SERVER_IP` 改成这台电脑的局域网 IP，触发一次红外：
- UDP 版看到 `MOTION count=1 ... (received 10/10 so far)` 就说明通了，
  其余 9 次会显示为 `duplicate ignored`
- TCP 版看到 `MOTION count=1 ... path=/motion` 就说明通了

## 设计说明

- **不用异步**：只有一个红外模块、一个上报动作，阻塞主循环最简单，
  也少一堆 `'static` 和任务划分的麻烦。
- **上报有三套**：UDP 版一条数据报发出去就行，不用建连、不用等响应，
  靠连发 10 次（间隔 0.5 秒）防丢包，接收端按 `count` 去重，
  10 次里收到 1 次就算成功，适合局域网里的自建接收端；
  TCP 版 `POST /motion` 发一条 JSON，有连接、有响应确认，
  一次触发只发一次，不用接收端去重，适合接收端是现成 HTTP 服务的情况；
  ntfy 版第一次推送前 DNS 解析一次服务器 IP 并缓存（之后推送直接用
  缓存的 IP，连接层失败才重新解析），再走 TLS 1.3 `POST /motion`，
  Bearer token 认证，一次触发发一次，手机上的 ntfy 应用直接收到推送，
  板子直连 ntfy 服务器、不经过任何中转，适合要手机实时提醒的情况。
  切换只改 `TRANSPORT` 这一行，三套代码都留着。
  ntfy 版的 TLS 记录缓冲（两个 8 KiB）放堆上：堆是 128 KiB，
  Wi-Fi 约占 50~60 KiB，握手时再加这两个缓冲和零星分配，够用；
  堆大小没动过，如果以后报堆不够，先看日志里的实际占用再定。
- **接收端怎么处理（去重是核心）**：去重键是 `(来源 IP, count)`，
  `count` 是设备上每次触发 +1 的全局序号。120 秒内见过的算重复，
  直接丢弃；`count` 比已处理的小，是迟到的旧包，也丢弃——但这条
  只在 120 秒内有效，超过 120 秒没收到包就认为是设备重启了，
  小 `count` 按新事件处理。详见 `tools/test_server.py`。
- **CPU 不休眠，只用 modem-sleep**：`PowerSaveMode::MinModem`，
  平时射频间歇关，只在 DTIM 时醒来收 beacon；发包时立刻唤醒，
  毫秒级发出。红外模块本身也要持续供电，省电的瓶颈不在 CPU。
- **HC-SR501 上电要稳定约 30 秒**：程序启动后会先等 `WARMUP_MS`，
  期间的误触发会被忽略。
- **去重**：HC-SR501 触发后输出会保持高电平几秒，程序等它落下
  再加 500 ms 保护间隔才重新布防，一个人走过只发一条。
- **Wi-Fi 断线会重连**：每次上报前检查 `is_connected()`，
  断了就重新 `connect()`。

## 版本基线（重要）

这套依赖是 2026-06 在 ESP32-C3 SuperMini 上实测过的组合
（Derek Molloy 的 C3 + esp-radio 0.18 教程），不要混用新旧 API：

- `esp-hal ~1.1` / `esp-radio 0.18` / `esp-rtos 0.3` / `smoltcp 0.13`
- `esp-radio::wifi::new()` 在 0.18 里不再要 `esp_radio::init()`，
  接口名是 `interfaces.station`（不是旧版的 `interfaces.sta`），
  配置是 `Config::Station(StationConfig)`（不是旧版的 `ModeConfig::Client`）

## 诚实说明：这份模板没有在本机编译过

我这台电脑上没有 Rust 工具链，没法替你 `cargo check`。
写之前核对了上面那份实测教程和 smoltcp 0.13 的文档，
但有三处 API 是按文档推断的，如果编译报错先看这三处：

1. `PowerSaveMode::MinModem` —— 枚举变体名，按 ESP-IDF 的
   `WIFI_PS_MIN_MODEM` 对应；报错就去 docs.rs 查
   `esp_radio::wifi::PowerSaveMode` 的实际变体名
   （`None` 一定存在，实在不行先改成 `None` 跑通再说）
2. `device.mac_address()` —— 0.18 的 `Interface` 上的方法名，
   旧版 `WifiDevice` 上叫这个；如果没了，改从
   `esp_hal::efuse` 或控制器侧取 MAC
3. `dhcpv4::Socket::poll()` 返回的 `Event::Configured(config)` ——
   smoltcp 0.13 的事件形状；如果签名变了，按编译器提示调整
4. UDP 的 `bind()` 和 `send_slice()` —— smoltcp 0.13 里 `send_slice(data, endpoint)`
   发包前要先 `bind()` 一个本地端口，模板里绑的是固定端口 49152；
   如果 `bind` 要求别的参数形状，按编译器提示调整

先跑 `cargo check`，把报错贴给我，我再帮你改。

## 修改记录

- 2026-11-05：初版，TCP+HTTP 上报（`POST /motion`，一次触发发一次）
- 2026-11-05：改成 UDP 上报，一次触发连发 10 次（间隔 0.5 秒），
  接收端按 `count` 去重；验证时发现接收端有个 bug——设备重启后
  `count` 从 0 开始会被误判为迟到的旧包扔掉，已修成 120 秒超时重置
- 2026-11-05：TCP 和 UDP 两套都保留，改 `TRANSPORT` 一行切换，
  默认 UDP；`tools/test_server.py` 支持 `udp` / `tcp` 两种模式
- 2026-11-07：在 InstaBox 免费机（Rust 1.99.1）上跑通 `cargo check`，按 esp-radio 0.18 / smoltcp 0.13.1 的真实 API 修了三轮：① 删 esp-radio 的 `smoltcp` feature（0.18 已删），net.rs 用 Interface 的 token API 自己包了一层 WifiDevice 实现 smoltcp Device；`PowerSaveMode::MinModem` → `Minimum`；删 `controller.start()`；`connect()` → `block_on(connect_async())`；`is_connected()` 按 bool 处理；② `udp::SocketBuffer` → `udp::PacketBuffer`、`SocketSet::new` 改传切片、TCP connect 补 `iface.context()`；③ UDP 的 `PacketBuffer::new` 需元数据+载荷两个缓冲，main.rs 加 `udp_rx_metadata` / `udp_tx_metadata`（各 16 个 `PacketMetadata::EMPTY`）。另加 `embassy-futures = "0.1"`（只用来跑 connect_async）和 `Cargo.lock`（锁定通过 check 的版本）。注意：check 通过但有 3 个 warning，其中 `wait_for_high` / `wait_for_low` 是 async 方法、直接调用不会等待（esp-hal 1.x 的 API 形态），真机上要确认是否需要 block_on。
- 2026-11-07：修红外等待问题——查 esp-hal 1.1.2 源码确认 `wait_for_high` / `wait_for_low` 只有 async 版、无阻塞版，main.rs 改为 `embassy_futures::block_on(pir.wait_for_high())` / `block_on(pir.wait_for_low())`（与 net.rs 里包 `connect_async()` 同一做法）；`cargo check` 通过，等待相关的 warning 已消失，只剩 `Transport::Tcp` 未构造这一个无害 warning（TCP 是按需保留的备用路线）。
- 2026-11-07：补链接参数——`.cargo/config.toml` 的 rustflags 加 `"-C", "link-arg=-Tlinkall.x"`（esp-hal 的标准做法，`linkall.x` 由 esp-hal 的 build 脚本生成）。之前缺了这行，`cargo check` 能过但裸 `cargo build` 在链接阶段报 20 个 undefined symbol（`_stack_start`、`DefaultHandler`、`WIFI_BB` 等）；补上后在 InstaBox 免费机（Rust 1.99.1）上裸 `cargo build` 一次通过，只剩 `Transport::Tcp` 未构造这一个无害 warning。
- 2026-11-18：加 ntfy 推送通道（`Transport::Ntfy`，默认仍是 UDP，切换只改 `TRANSPORT` 一行）：smoltcp 开 `socket-dns` feature，net.rs 加 DNS 套接字（套接字集合 3 个扩到 4 个，DNS 服务器取 DHCP 给的、没给 fallback 8.8.8.8），`resolve()` 查 A 记录，解析结果缓存到 `Net.ntfy_ip`（第一次推送前解析一次、之后推送直接用缓存的 IP，只有推送在连接层失败时才清掉缓存、下次推送重新解析，DNS 的时间成本每次上电只付一次）；`send_ntfy()` 用 `smoltcp-http-client`（git 依赖，rev 钉在 2c04554，`tls` feature）走 TLS 1.3 POST `https://<解析出的IP>/motion`，请求头 `Authorization: Bearer <NTFY_TOKEN>`（编译时环境变量传入）+ `Title: 有人经过`，body 是纯文本 `Motion #N detected`；根证书 `assets/isrg-root-x1.der`（Let's Encrypt ISRG Root X1，2026-11-18 在 InstaBox 上用 openssl 验过证书链、和官网 DER 逐字节对过）；硬件随机数用 esp-hal 的 `Rng::new()`（1.1 版不取外设参数）包一层 `HwRng` 实现 `CryptoRng`；TLS 记录缓冲两个 8 KiB 放堆上（堆 128 KiB，Wi-Fi 约占 50~60 KiB，够用，没动堆大小）；Net 实现 `TcpTransport` trait（透传现成的 TCP 套接字操作），`main.rs` 传 `delay` 进 Net。在 InstaBox（Rust 1.100.0）上 `cargo check` 通过。**只在 InstaBox 上 cargo check 通过、未在真机上验证：DNS 解析、TLS 握手、实际推送到手机都要烧录实测。**
