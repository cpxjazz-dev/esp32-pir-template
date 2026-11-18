//! 联网部分：Wi-Fi 连接、DHCP、上报。
//!
//! 之所以单独放一个文件，是因为 no_std 下没有 `std::net`，
//! 每一层都得自己搭：Wi-Fi 连接、DHCP 拿 IP、发包。
//! 代码量全在这里，主程序 `main.rs` 只剩红外逻辑，看着就短了。
//!
//! 上报有三套，切换只要改 `TRANSPORT` 这一行：
//!   - UDP：一条数据包发出去就行，不用建连、不用等响应，
//!     一次触发连发 10 次（间隔 0.5 秒），接收端按 count 去重，
//!     10 次里收到 1 次就算成功。代码短，适合局域网里的自建接收端。
//!   - TCP+HTTP：`POST /motion` 发一条 JSON，有连接、有响应确认，
//!     适合接收端是现成 HTTP 服务（比如家里的服务器、webhook）的
//!     情况。代码长不少，但不用接收端自己写去重。
//!   - ntfy：第一次推送前 DNS 解析出 ntfy 服务器的 IP 并缓存起来，
//!     之后每次推送直接用缓存的 IP（DNS 的时间成本每次上电只付一次）；
//!     只有推送在连接层失败时才清掉缓存、下次推送重新解析
//!     （应对 Cloudflare 换 IP）。用 `smoltcp-http-client`
//!     走 TLS 1.3 `POST /motion`，Bearer token 认证，
//!     手机上的 ntfy 应用直接收到推送。不经过任何中转，
//!     板子直连 ntfy 服务器。

use alloc::format;
use alloc::string::ToString;
use esp_hal::delay::Delay;
use esp_hal::rng::Rng as EspRng;
use esp_hal::time::{Duration, Instant};
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config, Interface as WifiInterface, PowerSaveMode, WifiController};
use rand_core::{CryptoRng, Error as RandError, RngCore};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{
    Device as SmolDevice, DeviceCapabilities, Medium, RxToken as SmolRxToken, TxToken as SmolTxToken,
};
use smoltcp::socket::dhcpv4;
use smoltcp::socket::dns;
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use smoltcp_http_client::tls::TLS_RECORD_BUFFER_SIZE;
use smoltcp_http_client::{Error as HttpError, HttpClient, Request, TcpTransport, TlsOptions};

extern crate alloc;

// ================= 需要你改的配置 =================

/// 上报走哪条路：接收端能用哪个就改成哪个，改这一行就行
const TRANSPORT: Transport = Transport::Udp;

/// Wi-Fi 名字和密码：编译时用环境变量传入，不要写死提交
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

/// 接收端服务器的 IPv4 地址（模板不用 DNS，直接写 IP）
const SERVER_IP: [u8; 4] = [192, 168, 1, 100];
const SERVER_PORT: u16 = 8080;
/// TCP 版的请求路径（UDP 版不用）
const SERVER_PATH: &str = "/motion";
/// UDP 版：一次触发连发几次，每次间隔多少毫秒
const BURST_COUNT: u32 = 10;
const BURST_INTERVAL_MS: u32 = 500;
/// UDP 版的本地端口（固定一个就行，不用随机端口）
const LOCAL_PORT: u16 = 49152;

/// ntfy 服务器：域名、端口、topic 路径。
/// 注意：URL 里写 IP（smoltcp-http-client 没有 DNS），
/// server_name 写域名（SNI + 证书主机名校验），两者是两回事；
/// IP 由下面的 resolve() 每次上报前现解析。
const NTFY_HOST: &str = "ntfy-jagfm94g-cpxjazz-dev.cloady.io";
const NTFY_PORT: u16 = 443;
const NTFY_PATH: &str = "/motion";
/// ntfy 的 Bearer token：编译时用环境变量传入，不要写死提交。
/// 这个 token 是只对 motion topic 只写的普通用户（非 admin），
/// 泄露了也只能往这一个 topic 发消息。
const NTFY_TOKEN: &str = env!("NTFY_TOKEN");
/// ISRG Root X1（Let's Encrypt 根证书，DER，2035 年到期）。
/// ntfy 服务器的证书链是 leaf ← YE2 ← Root YE ← ISRG Root X2
/// ← ISRG Root X1，根就是 ISRG Root X1（2026-11-18 在 InstaBox
/// 上用 openssl 验过链，也和官网下载的 DER 逐字节对过）。
const NTFY_CA_DER: &[u8] = include_bytes!("../assets/isrg-root-x1.der");
/// DHCP 没给 DNS 服务器时 fallback 到这个。
const DNS_FALLBACK: IpAddress = IpAddress::v4(8, 8, 8, 8);
/// 一次 DNS 查询最多等多久。
const DNS_TIMEOUT_MS: u64 = 5_000;

// ================= 联网状态 =================

/// 上报走 UDP、TCP+HTTP 还是 ntfy
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
    Ntfy,
}

// ================= smoltcp 设备适配 =================
//
// esp-radio 0.18 不再提供 `smoltcp` feature，`Interface` 也不再实现
// smoltcp 的 Device trait。0.18 只暴露了底层的 token API：
//   Interface::receive() -> Option<(WifiRxToken, WifiTxToken)>
//   Interface::transmit() -> Option<WifiTxToken>
//   WifiRxToken::consume_token(f: FnOnce(&mut [u8]))
//   WifiTxToken::consume_token(len, f: FnOnce(&mut [u8]))
// 下面这层薄薄的适配把它们包成 smoltcp::phy::Device，
// 其余代码（DHCP/UDP/TCP）不用动。

/// 把 esp-radio 0.18 的 Wi-Fi 接口包成 smoltcp 的网卡。
pub struct WifiDevice<'d> {
    inner: WifiInterface<'d>,
}

impl<'d> WifiDevice<'d> {
    pub fn new(interface: WifiInterface<'d>) -> Self {
        Self { inner: interface }
    }
}

/// smoltcp 的收包 token：包在 esp-radio 的 WifiRxToken 外面。
pub struct WifiRxToken(esp_radio::wifi::WifiRxToken);

impl SmolRxToken for WifiRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        // esp-radio 给的是 &mut [u8]，smoltcp 只要 &[u8]，借一次不可变引用即可。
        self.0.consume_token(|buf| f(&*buf))
    }
}

/// smoltcp 的发包 token：包在 esp-radio 的 WifiTxToken 外面。
pub struct WifiTxToken(esp_radio::wifi::WifiTxToken);

impl SmolTxToken for WifiTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.0.consume_token(len, f)
    }
}

impl SmolDevice for WifiDevice<'_> {
    type RxToken<'a> = WifiRxToken where Self: 'a;
    type TxToken<'a> = WifiTxToken where Self: 'a;

    fn receive(&mut self, _timestamp: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.inner
            .receive()
            .map(|(rx, tx)| (WifiRxToken(rx), WifiTxToken(tx)))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        self.inner.transmit().map(WifiTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        // esp-radio 的发包缓冲是 MTU（默认 1500）字节；报 1500 保证
        // smoltcp 永远不会要求一个比底层缓冲更大的帧。
        caps.max_transmission_unit = 1500;
        caps
    }
}

/// 持有 smoltcp 接口、Wi-Fi 设备和套接字集合。
/// 套接字的存储区借自 main() 里的局部数组，所以 Net 带生命周期。
pub struct Net<'a> {
    iface: Interface,
    device: WifiDevice<'static>,
    sockets: SocketSet<'a>,
    dhcp_handle: SocketHandle,
    udp_handle: SocketHandle,
    tcp_handle: SocketHandle,
    dns_handle: SocketHandle,
    /// DHCP 给的 DNS 服务器（一般是路由器）；没给就是 None，
    /// 解析时 fallback 到 8.8.8.8。
    dns_server: Option<IpAddress>,
    /// ntfy 服务器的 IP 缓存：第一次推送前 DNS 解析一次存下来，
    /// 之后推送直接用它，不再解析；推送在连接层失败时清掉，
    /// 下次推送重新解析（应对 Cloudflare 换 IP）。
    /// None = 还没解析过（或已被清掉）。
    ntfy_ip: Option<IpAddress>,
    /// TcpTransport::delay_ms 要用，存在 Net 里（Delay 是单位结构体，
    /// Copy，传值进来 main() 那边还能继续用自己的那份）。
    delay: Delay,
}

impl<'a> Net<'a> {
    pub fn new(
        device: WifiInterface<'static>,
        socket_entries: &'a mut [SocketStorage<'a>; 4],
        udp_rx_storage: &'a mut [u8],
        udp_tx_storage: &'a mut [u8],
        udp_rx_metadata: &'a mut [udp::PacketMetadata],
        udp_tx_metadata: &'a mut [udp::PacketMetadata],
        tcp_rx_storage: &'a mut [u8],
        tcp_tx_storage: &'a mut [u8],
        dns_queries: &'a mut [Option<dns::DnsQuery>],
        delay: Delay,
    ) -> Self {
        // 注意：这里直接把 &'a mut 交过去，不要再 &mut [..] 重借，
        // 否则生命周期会被缩短，SocketSet 就拿不到 'a。
        // 但 smoltcp 0.13 的 SocketSet::new 只接受切片引用不接受数组引用，
        // 所以用 `as` 做一次定长数组到切片的无尺寸强制转换（不产生重借，
        // 'a 生命周期原样保留）。
        let mut sockets = SocketSet::new(socket_entries as &mut [SocketStorage<'a>]);
        let dhcp_handle = sockets.add(dhcpv4::Socket::new());
        let udp_handle = sockets.add(udp::Socket::new(
            udp::PacketBuffer::new(udp_rx_metadata, udp_rx_storage),
            udp::PacketBuffer::new(udp_tx_metadata, udp_tx_storage),
        ));
        let tcp_handle = sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(tcp_rx_storage),
            tcp::SocketBuffer::new(tcp_tx_storage),
        ));
        // DNS 套接字：查询槽位借自 main() 的数组（2 个槽，一次只查一个，
        // 够用）。服务器列表现在还不知道（DHCP 没跑），先传空，
        // obtain_ip() 拿到 DHCP 的 dns_servers 后再 update_servers()。
        let dns_handle = sockets.add(dns::Socket::new(&[], dns_queries));

        let mac = device.mac_address();
        let mut device = WifiDevice::new(device);
        let iface = Interface::new(
            IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress::from_bytes(&mac))),
            &mut device,
            smol_timestamp(),
        );

        let mut net = Self {
            iface,
            device,
            sockets,
            dhcp_handle,
            udp_handle,
            tcp_handle,
            dns_handle,
            dns_server: None,
            ntfy_ip: None,
            delay,
        };

        // UDP 发包前要先 bind 一个本地端口，绑一次就行；
        // 绑在固定端口上，不用随机端口，接收端也好认。
        net.sockets
            .get_mut::<udp::Socket>(net.udp_handle)
            .bind((IpAddress::v4(0, 0, 0, 0), LOCAL_PORT))
            .expect("Failed to bind UDP socket");

        net
    }

    /// 配好 STA 模式、开 modem-sleep、启动 Wi-Fi。
    ///
    /// 注意：0.18 里 `set_config()` 内部已经（重新）启动控制器了，
    /// 不用再调 `start()`（这个方法在 0.18 里已经不存在）。
    /// `PowerSaveMode::MinModem` 在 0.18 里改名叫 `Minimum`。
    pub fn configure_wifi(controller: &mut WifiController<'_>) {
        let station_config = StationConfig::default()
            .with_ssid(SSID)
            .with_password(PASSWORD.to_string());
        let radio_config = Config::Station(station_config);
        controller.set_config(&radio_config).unwrap();

        // modem-sleep：平时射频间歇关，只在 DTIM 时醒来收 beacon；
        // 发包时立刻唤醒，毫秒级发出。CPU 不休眠。
        // 注意：set_power_saving() 要在 set_config() 之后调。
        controller
            .set_power_saving(PowerSaveMode::Minimum)
            .unwrap();

        println!("Wi-Fi started");
    }

    /// 连上 Wi-Fi，一直重试直到连上为止。
    ///
    /// 注意：esp-radio 0.18 没有同步的 `connect()`，只有 `connect_async()`。
    /// 这里用 `embassy_futures::block_on` 把它跑完——主循环仍是阻塞式的，
    /// 只是连接这一下在原地忙等事件；esp-rtos 是抢占式的，射频后台任务
    /// 不会因此饿死。
    pub fn connect_wifi(controller: &mut WifiController<'_>, delay: &Delay) {
        loop {
            println!("Connecting to SSID={} ...", SSID);
            match embassy_futures::block_on(controller.connect_async()) {
                Ok(_info) => {
                    println!("Wi-Fi connected");
                    return;
                }
                Err(e) => {
                    println!("connect_async() error: {:?}, retrying in 5 s", e);
                    delay.delay_millis(5000);
                }
            }
        }
    }

    /// 上报前检查链路，断了就重连。
    /// 注意：0.18 的 is_connected() 直接返回 bool，不再是 Result。
    pub fn ensure_connected(&mut self, controller: &mut WifiController<'_>, delay: &Delay) {
        if !controller.is_connected() {
            println!("Wi-Fi link lost, reconnecting ...");
            Self::connect_wifi(controller, delay);
        }
    }

    /// 等 DHCP 分到 IP，配到接口上
    pub fn obtain_ip(&mut self, delay: &Delay) {
        println!("Waiting for DHCP lease ...");
        loop {
            self.iface
                .poll(smol_timestamp(), &mut self.device, &mut self.sockets);

            if let Some(event) = self
                .sockets
                .get_mut::<dhcpv4::Socket>(self.dhcp_handle)
                .poll()
            {
                if let dhcpv4::Event::Configured(config) = event {
                    println!("DHCP: address={}, router={:?}", config.address, config.router);
                    self.iface.update_ip_addrs(|addrs| {
                        // 启动时地址表是空的，直接 push 即可
                        addrs.push(IpCidr::Ipv4(config.address)).unwrap();
                    });
                    if let Some(router) = config.router {
                        self.iface
                            .routes_mut()
                            .add_default_ipv4_route(router)
                            .unwrap();
                    }
                    // DHCP 一般还会给 DNS 服务器（通常是路由器）；
                    // 没给的话 dns_server 保持 None，解析时用 8.8.8.8。
                    // 注意：smoltcp 0.13 的 DNS_MAX_SERVER_COUNT = 1，
                    // update_servers() 只留第一个，多了会截断。
                    if let Some(dns) = config.dns_servers.first() {
                        let server = IpAddress::Ipv4(*dns);
                        println!("DHCP: dns server={:?}", server);
                        self.dns_server = Some(server);
                        self.sockets
                            .get_mut::<dns::Socket>(self.dns_handle)
                            .update_servers(&[server]);
                    }
                    return;
                }
            }

            delay.delay_millis(50);
        }
    }

    /// 上报一次触发，按 TRANSPORT 走 UDP、TCP 或 ntfy。
    /// 失败只打印日志，不 panic，下次触发再试。
    pub fn send_notification(&mut self, count: u32, delay: &Delay) {
        match TRANSPORT {
            Transport::Udp => self.send_udp(count, delay),
            Transport::Tcp => self.send_tcp(count, delay),
            Transport::Ntfy => self.send_ntfy(count, delay),
        }
    }

    /// 把域名解析成 IPv4（只查 A 记录）。
    /// 服务器用 DHCP 给的 DNS，没给就用 8.8.8.8。
    /// 失败只打印日志、返回 None，不 panic。
    fn resolve(&mut self, hostname: &str, delay: &Delay) -> Option<IpAddress> {
        let server = self.dns_server.unwrap_or(DNS_FALLBACK);
        self.sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .update_servers(&[server]);

        let handle = match self.sockets.get_mut::<dns::Socket>(self.dns_handle).start_query(
            self.iface.context(),
            hostname,
            DnsQueryType::A,
        ) {
            Ok(h) => h,
            Err(e) => {
                println!("DNS start_query({}) error: {:?}", hostname, e);
                return None;
            }
        };

        let deadline = Instant::now() + Duration::from_millis(DNS_TIMEOUT_MS);
        loop {
            self.iface
                .poll(smol_timestamp(), &mut self.device, &mut self.sockets);
            match self
                .sockets
                .get_mut::<dns::Socket>(self.dns_handle)
                .get_query_result(handle)
            {
                Ok(addrs) => {
                    // DNS_MAX_RESULT_COUNT = 1，最多回来 1 条；
                    // A 查询只会返回 IPv4，直接取第一条。
                    let ip = addrs.first().copied();
                    println!("DNS {} -> {:?}", hostname, ip);
                    return ip;
                }
                Err(dns::GetQueryResultError::Pending) => {}
                Err(dns::GetQueryResultError::Failed) => {
                    println!("DNS query({}) failed", hostname);
                    return None;
                }
            }
            if Instant::now() > deadline {
                println!("DNS query({}) timed out", hostname);
                self.sockets
                    .get_mut::<dns::Socket>(self.dns_handle)
                    .cancel_query(handle);
                return None;
            }
            delay.delay_millis(10);
        }
    }

    /// ntfy 服务器的 IP（带缓存）：有缓存直接用，没有就 DNS 解析一次
    /// 存下来。解析失败返回 None，不 panic——调用方跳过本次推送，
    /// 下次触发再解析。
    fn ntfy_ip(&mut self, delay: &Delay) -> Option<IpAddress> {
        if let Some(ip) = self.ntfy_ip {
            return Some(ip);
        }
        let ip = self.resolve(NTFY_HOST, delay)?;
        self.ntfy_ip = Some(ip);
        Some(ip)
    }

    /// ntfy 版：用缓存的 ntfy 服务器 IP（第一次推送前 DNS 解析一次），
    /// 再用 smoltcp-http-client 走 TLS 1.3 POST /motion。
    /// 一次触发只发一次；失败只打印日志，不 panic，下次触发再试。
    /// 推送在连接层失败（连不上、握手失败等）时清掉 IP 缓存，
    /// 下次推送重新解析；服务端正常回了响应（哪怕是 4xx/5xx）
    /// 说明 IP 是通的，不清缓存。
    ///
    /// 注意：URL 里写解析出的 IP（crate 没有 DNS），
    /// server_name 写域名（SNI + 证书主机名校验）。
    fn send_ntfy(&mut self, count: u32, delay: &Delay) {
        // IP 有缓存直接用，没有才解析一次（每次上电只付一次 DNS 时间）。
        let Some(ip) = self.ntfy_ip(delay) else {
            println!("ntfy: DNS resolve failed, skipping");
            return;
        };
        let url = format!("https://{}:{}{}", ip, NTFY_PORT, NTFY_PATH);
        let body = format!("Motion #{} detected", count);
        let auth = format!("Bearer {}", NTFY_TOKEN);

        // TLS 记录缓冲：两个 8 KiB。
        // 放堆上，不放栈上：main 任务的栈本来已经放了套接字缓冲，
        // 再加 16 KiB 有溢出的风险；堆是 128 KiB，Wi-Fi 约占 50~60 KiB，
        // 握手时再加这两个缓冲约 16 KiB + 零星分配，够用。
        // 注意：vec! 是直接在堆上分配再填充，不会先在栈上造一个大数组。
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_BUFFER_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_BUFFER_SIZE];
        let mut hw_rng = HwRng(EspRng::new());

        let mut client = HttpClient::new(&mut *self);
        let req = Request::post(&url, "text/plain", body.as_bytes())
            .header("Authorization", &auth)
            .header("Title", "有人经过");
        let tls_opts = TlsOptions {
            server_name: NTFY_HOST,
            ca_der: NTFY_CA_DER,
            rng: &mut hw_rng,
            read_buffer: &mut read_buf,
            write_buffer: &mut write_buf,
        };
        match client.request_tls(&req, tls_opts) {
            Ok(resp) => {
                println!(
                    "ntfy POST ok: status={} body={}",
                    resp.status,
                    resp.body_str().unwrap_or("<non-utf8>")
                );
            }
            Err(e) => {
                println!("ntfy POST failed: {}", e);
                // 连接层失败（连不上 / 握手失败 / 收发失败 / 超时）：
                // 可能是缓存的 IP 已过期（Cloudflare 换 IP），清掉缓存，
                // 下次推送重新解析。服务端已经回了响应的错误
                //（InvalidResponse / BodyTooLarge 等）说明 IP 是通的，
                // 不清缓存。
                if matches!(
                    e,
                    HttpError::Connect
                        | HttpError::ConnectTimeout
                        | HttpError::Send
                        | HttpError::Recv
                        | HttpError::ResponseTimeout
                        | HttpError::Tls(_)
                ) {
                    self.ntfy_ip = None;
                    println!("ntfy: connection failed, cached IP cleared, will re-resolve next push");
                }
            }
        }
    }

    /// UDP 版：同一个包连发 BURST_COUNT 次，间隔 BURST_INTERVAL_MS。
    /// UDP 不保证送达，靠“连发 + 接收端去重”补：10 次里收到 1 次就算成功，
    /// 其余 9 次接收端按 count 认出来扔掉。
    fn send_udp(&mut self, count: u32, delay: &Delay) {
        let uptime_ms = Instant::now().duration_since_epoch().as_millis();
        let payload = format!(
            r#"{{"event":"motion","count":{},"uptime_ms":{}}}"#,
            count, uptime_ms
        );
        let remote = (IpAddress::v4(SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3]), SERVER_PORT);

        for attempt in 1..=BURST_COUNT {
            {
                let socket = self.sockets.get_mut::<udp::Socket>(self.udp_handle);
                match socket.send_slice(payload.as_bytes(), remote) {
                    Ok(()) => {}
                    Err(e) => {
                        println!("UDP send error (attempt {}): {:?}", attempt, e);
                    }
                }
            }
            // send_slice 只是把包排进队列，poll 几下才真正发出去
            for _ in 0..5 {
                self.iface
                    .poll(smol_timestamp(), &mut self.device, &mut self.sockets);
                delay.delay_millis(5);
            }
            println!("UDP burst {}/{}: count={}", attempt, BURST_COUNT, count);
            if attempt < BURST_COUNT {
                delay.delay_millis(BURST_INTERVAL_MS);
            }
        }
    }

    /// TCP 版：POST 一条 JSON 到 SERVER_PATH，一次触发只发一次，
    /// 有连接、有响应确认，不用接收端去重。
    fn send_tcp(&mut self, count: u32, delay: &Delay) {
        let body = format!(r#"{{"event":"motion","count":{}}}"#, count);
        let request = format!(
            "POST {} HTTP/1.0\r\nHost: {}.{}.{}.{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            SERVER_PATH,
            SERVER_IP[0],
            SERVER_IP[1],
            SERVER_IP[2],
            SERVER_IP[3],
            body.len(),
            body,
        );
        let remote = IpAddress::v4(SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3]);

        // 1. 建立 TCP 连接。
        // 注意：smoltcp 0.13 的 connect 要传 iface.context() 做第一个参数，
        // 本地端口直接给 u16 即可（0.13 的 connect 签名是
        // connect(cx, remote_endpoint, local_endpoint)）。
        {
            let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
            if let Err(e) = socket.connect(self.iface.context(), (remote, SERVER_PORT), 49152) {
                println!("TCP connect error: {:?}", e);
                return;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.iface
                .poll(smol_timestamp(), &mut self.device, &mut self.sockets);
            let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
            if socket.may_send() {
                break;
            }
            if Instant::now() > deadline {
                println!("TCP connect timed out");
                socket.abort();
                return;
            }
            delay.delay_millis(10);
        }

        // 2. 发请求（可能一次写不完，循环写完为止）
        let request_bytes = request.as_bytes();
        let mut sent = 0;
        while sent < request_bytes.len() {
            self.iface
                .poll(smol_timestamp(), &mut self.device, &mut self.sockets);
            let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
            if !socket.may_send() {
                println!("TCP closed while sending");
                return;
            }
            match socket.send_slice(&request_bytes[sent..]) {
                Ok(0) => delay.delay_millis(10),
                Ok(n) => sent += n,
                Err(e) => {
                    println!("TCP send error: {:?}", e);
                    socket.abort();
                    return;
                }
            }
        }
        println!(
            "Sent {} bytes to {}:{}{}",
            sent, remote, SERVER_PORT, SERVER_PATH
        );

        // 3. 读响应，直到对方关闭或超时（只打印，不解析）
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 512];
        loop {
            self.iface
                .poll(smol_timestamp(), &mut self.device, &mut self.sockets);
            let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
            if socket.can_recv() {
                match socket.recv_slice(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        println!(
                            "Response: {}",
                            core::str::from_utf8(&buf[..n]).unwrap_or("<non-utf8>")
                        );
                    }
                    Err(_) => break,
                }
            }
            if !socket.may_recv() {
                break;
            }
            if Instant::now() > deadline {
                println!("Response read timed out");
                break;
            }
            delay.delay_millis(10);
        }

        // 4. 关闭连接，把 FIN 发出去
        {
            let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
            let _ = socket.close();
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            self.iface
                .poll(smol_timestamp(), &mut self.device, &mut self.sockets);
            delay.delay_millis(10);
        }
    }
}

// ================= 给 Net 实现 TcpTransport，HttpClient 就能用了 =================
//
// 只是把 Net 里现成的 smoltcp 操作透传出来（和
// smoltcp-http-client 的 examples/post_json.rs 里的做法一样）。

impl TcpTransport for Net<'_> {
    fn now_ms(&mut self) -> u64 {
        Instant::now().duration_since_epoch().as_millis() as u64
    }

    fn poll(&mut self) {
        self.iface
            .poll(smol_timestamp(), &mut self.device, &mut self.sockets);
    }

    fn delay_ms(&mut self, ms: u32) {
        self.delay.delay_millis(ms);
    }

    fn connect(&mut self, remote: (IpAddress, u16), local_port: u16) -> Result<(), HttpError> {
        let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
        socket
            .connect(self.iface.context(), remote, local_port)
            .map_err(|_| HttpError::Connect)
    }

    fn send(&mut self, data: &[u8]) -> Result<usize, HttpError> {
        let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
        socket.send_slice(data).map_err(|_| HttpError::Send)
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, HttpError> {
        let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
        socket.recv_slice(buf).map_err(|_| HttpError::Recv)
    }

    fn may_send(&mut self) -> bool {
        self.sockets.get_mut::<tcp::Socket>(self.tcp_handle).may_send()
    }

    fn can_recv(&mut self) -> bool {
        self.sockets.get_mut::<tcp::Socket>(self.tcp_handle).can_recv()
    }

    fn may_recv(&mut self) -> bool {
        self.sockets.get_mut::<tcp::Socket>(self.tcp_handle).may_recv()
    }

    fn close(&mut self) {
        let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp_handle);
        let _ = socket.close();
    }

    fn abort(&mut self) {
        self.sockets.get_mut::<tcp::Socket>(self.tcp_handle).abort();
    }
}

/// 硬件随机数：esp-hal 的 Rng 只实现了 rand_core 0.6 的 RngCore，
/// 没实现 CryptoRng，不能直接给 TlsOptions 用（embedded-tls 0.19
/// 要的是 RngCore + CryptoRng）。包一层 newtype 转发即可：
/// 底层熵源还是片上硬件 RNG（和 smoltcp-http-client 的
/// post_json_tls 例子里的做法一样；esp-hal 1.1 的 Rng::new()
/// 不取外设参数，RNG 是单例，直接 new）。
pub struct HwRng(EspRng);

impl RngCore for HwRng {
    fn next_u32(&mut self) -> u32 {
        self.0.random()
    }

    fn next_u64(&mut self) -> u64 {
        ((self.0.random() as u64) << 32) | (self.0.random() as u64)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let word = self.0.random().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), RandError> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for HwRng {}

/// 把 esp-hal 的时间转成 smoltcp 的时间
fn smol_timestamp() -> SmolInstant {
    SmolInstant::from_millis(Instant::now().duration_since_epoch().as_millis() as i64)
}
