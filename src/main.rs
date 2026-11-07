//! ESP32-C3 + HC-SR501 红外感应 → Wi-Fi 上报模板
//!
//! 阻塞式主循环，不用 Embassy / 异步：
//!   等红外变高 → UDP 连发 10 次 → 等红外变低 → 继续等
//!
//! 联网的脏活全在 `net.rs` 里（Wi-Fi 连接、DHCP、UDP 发包），
//! 这里只剩红外逻辑。
//!
//! 接线：
//!   HC-SR501 VCC → 开发板 5V 脚（不要接 3.3V，也不要用 GPIO 供电）
//!   HC-SR501 GND → GND（和 C3 共地）
//!   HC-SR501 OUT → GPIO10
//!
//! 版本基线：esp-hal ~1.1 / esp-radio 0.18 / esp-rtos 0.3 / smoltcp 0.13
//! （2026-06 在 C3 SuperMini 上实测过的组合，见 README 的版本说明）

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

mod net;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use net::Net;

esp_bootloader_esp_idf::esp_app_desc!();

/// HC-SR501 上电稳定时间（毫秒），期间可能误触发，程序会等它过去
const WARMUP_MS: u32 = 30_000;
/// 触发后、输出变低前不再重复上报的保护间隔（毫秒）
const REARM_GUARD_MS: u32 = 500;

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Wi-Fi 要用堆：STA 联网约占 50~60 KB，再留余量给应用
    esp_alloc::heap_allocator!(size: 128 * 1024);

    let delay = Delay::new();

    // 红外信号脚：GPIO10。
    // 避开 strapping 脚 GPIO2/8/9、Flash 占用的 GPIO11~17、
    // USB-Serial-JTAG 的 GPIO18/19、UART0 的 GPIO20/21。
    // HC-SR501 输出平时为低，触发时变高，用下拉保持平时为低。
    let mut pir = Input::new(
        peripherals.GPIO10,
        InputConfig::default().with_pull(Pull::Down),
    );

    // 启动 esp-rtos：射频 blob 的后台任务挂在这里。
    // 这是给射频用的调度器，不是给你写异步任务用的；
    // start 之后 blob 自己在后台跑，主循环不用管它。
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // Wi-Fi 控制器 + 数据接口。
    // 0.18 的接口名是 interfaces.station（旧版叫 interfaces.sta），
    // wifi::new() 内部已经初始化了 radio，不用再调 esp_radio::init()。
    let (mut wifi_controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");
    let device = interfaces.station;

    // 联网状态：接口、设备、套接字集合都在 Net 里
    // （UDP 和 TCP 两套都建了，切换只改 net.rs 里的 TRANSPORT）
    let mut socket_entries: [smoltcp::iface::SocketStorage<'_>; 3] = Default::default();
    let mut udp_rx_storage = [0u8; 1024];
    let mut udp_tx_storage = [0u8; 1024];
    let mut udp_rx_metadata = [smoltcp::socket::udp::PacketMetadata::EMPTY; 16];
    let mut udp_tx_metadata = [smoltcp::socket::udp::PacketMetadata::EMPTY; 16];
    let mut tcp_rx_storage = [0u8; 2048];
    let mut tcp_tx_storage = [0u8; 2048];
    let mut net = Net::new(
        device,
        &mut socket_entries,
        &mut udp_rx_storage,
        &mut udp_tx_storage,
        &mut udp_rx_metadata,
        &mut udp_tx_metadata,
        &mut tcp_rx_storage,
        &mut tcp_tx_storage,
    );

    Net::configure_wifi(&mut wifi_controller);
    Net::connect_wifi(&mut wifi_controller, &delay);
    net.obtain_ip(&delay);

    println!("HC-SR501 warm-up: waiting {} ms ...", WARMUP_MS);
    delay.delay_millis(WARMUP_MS);
    println!("Ready. Waiting for motion on GPIO10 ...");

    let mut count: u32 = 0;
    loop {
        // 阻塞等红外变高。esp-hal 1.x 的 wait_for_high() 是 async 方法，
        // 没有阻塞版；直接调只会造出一个立即丢弃的 Future，等于没等。
        // 用 embassy-futures 的 block_on 在当前线程把它跑完（net.rs 里
        // 连 Wi-Fi 也是这么包的）。block_on 是忙轮询，esp-rtos 是抢占式
        // 的，射频 blob 照常跑，不会因为主循环堵在这里就饿死。
        embassy_futures::block_on(pir.wait_for_high());
        count += 1;
        println!("Motion #{} detected", count);

        net.ensure_connected(&mut wifi_controller, &delay);
        net.send_notification(count, &delay);

        // HC-SR501 触发后输出会保持高电平几秒：等它落下再加一点
        // 保护间隔，避免一个人走过连发多条。
        embassy_futures::block_on(pir.wait_for_low());
        delay.delay_millis(REARM_GUARD_MS);
    }
}
