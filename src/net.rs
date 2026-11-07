//! 联网部分：Wi-Fi 连接、DHCP、上报。
//!
//! 之所以单独放一个文件，是因为 no_std 下没有 `std::net`，
//! 每一层都得自己搭：Wi-Fi 连接、DHCP 拿 IP、发包。
//! 代码量全在这里，主程序 `main.rs` 只剩红外逻辑，看着就短了。
//!
//! 上报有两套，切换只要改 `TRANSPORT` 这一行：
//!   - UDP：一条数据包发出去就行，不用建连、不用等响应，
//!     一次触发连发 10 次（间隔 0.5 秒），接收端按 count 去重，
//!     10 次里收到 1 次就算成功。代码短，适合局域网里的自建接收端。
//!   - TCP+HTTP：`POST /motion` 发一条 JSON，有连接、有响应确认，
//!     适合接收端是现成 HTTP 服务（比如家里的服务器、webhook）的
//!     情况。代码长不少，但不用接收端自己写去重。

use alloc::format;
use alloc::string::ToString;
use esp_hal::delay::Delay;
use esp_hal::time::{Duration, Instant};
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config, Interface as WifiInterface, PowerSaveMode, WifiController};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{
    Device as SmolDevice, DeviceCapabilities, Medium, RxToken as SmolRxToken, TxToken as SmolTxToken,
};
use smoltcp::socket::dhcpv4;
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

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

// ================= 联网状态 =================

/// 上报走 UDP 还是 TCP+HTTP
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
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
}

impl<'a> Net<'a> {
    pub fn new(
        device: WifiInterface<'static>,
        socket_entries: &'a mut [SocketStorage<'a>; 3],
        udp_rx_storage: &'a mut [u8],
        udp_tx_storage: &'a mut [u8],
        udp_rx_metadata: &'a mut [udp::PacketMetadata],
        udp_tx_metadata: &'a mut [udp::PacketMetadata],
        tcp_rx_storage: &'a mut [u8],
        tcp_tx_storage: &'a mut [u8],
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
                    return;
                }
            }

            delay.delay_millis(50);
        }
    }

    /// 上报一次触发，按 TRANSPORT 走 UDP 或 TCP。
    /// 失败只打印日志，不 panic，下次触发再试。
    pub fn send_notification(&mut self, count: u32, delay: &Delay) {
        match TRANSPORT {
            Transport::Udp => self.send_udp(count, delay),
            Transport::Tcp => self.send_tcp(count, delay),
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

/// 把 esp-hal 的时间转成 smoltcp 的时间
fn smol_timestamp() -> SmolInstant {
    SmolInstant::from_millis(Instant::now().duration_since_epoch().as_millis() as i64)
}
