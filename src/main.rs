#![no_std]
#![no_main]
use core::net::SocketAddrV4;

use cortex_m::peripheral::MPU;
use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ipv4Address, Ipv4Cidr, StackResources, StaticConfigV4};
use embassy_net_driver::Driver;
use embassy_stm32::eth::{Ethernet, GenericPhy, PacketQueue, Sma};
use embassy_stm32::pac::{self, RIFSC, RISAF2};
use embassy_stm32::peripherals::{ETH_SMA, ETH1};
use embassy_stm32::rcc::{CpuClk, IcConfig, Icint, Icsel, Pll, Plldivm, Pllpdiv, Pllsel, SysClk};
use embassy_stm32::{Config, bind_interrupts, eth};
use embassy_time::{Duration, Instant, Ticker};
use embedded_io_async::{Read, Write};
use heapless::Vec;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    ETH1 => eth::InterruptHandler;
});

type Device = Ethernet<'static, ETH1, GenericPhy<Sma<'static, ETH_SMA>>>;

const PORT: u16 = 8000;
/// How long the board blasts when it is the sender (modes 'T' and 'B').
const DURATION: Duration = Duration::from_secs(10);
/// Per-write chunk handed to the TCP stack.
const CHUNK: usize = 4096;

// Static address for the board, /24. PC is the gateway / peer at .1.
const LOCAL_IP: Ipv4Address = Ipv4Address::new(192, 168, 0, 2);
const GATEWAY: Ipv4Address = Ipv4Address::new(192, 168, 0, 1);

fn rcc_config() -> Config {
    let mut config = Config::default();
    // PLL1 = HSI(64 MHz) / 4 * 50 = 800 MHz.
    config.rcc.pll1 = Some(Pll::Oscillator {
        source: Pllsel::Hsi,
        divm: Plldivm::Div4,
        fractional: 0,
        divn: 50,
        divp1: Pllpdiv::Div1,
        divp2: Pllpdiv::Div1,
    });
    config.rcc.ic1 = Some(IcConfig {
        source: Icsel::Pll1,
        divider: Icint::Div1,
    });
    let sys_ic = IcConfig {
        source: Icsel::Pll1,
        divider: Icint::Div4,
    };
    config.rcc.ic2 = Some(sys_ic);
    config.rcc.ic6 = Some(sys_ic);
    config.rcc.ic11 = Some(sys_ic);
    config.rcc.cpu = CpuClk::Ic1; // 800 MHz
    config.rcc.sys = SysClk::Ic2; // 200 MHz
    // Doesn't get past power supply config if supply is SMPS and voltage scale is Scale0.
    config.rcc.voltage_scale = embassy_stm32::rcc::VoltageScale::Scale1;
    config
}

/// 32-byte-aligned wrapper so the contained value can be covered exactly by an
/// ARMv8-M MPU region (region base/limit are 32-byte granular).
#[repr(C, align(32))]
struct Aligned<T>(T);

/// Mark `[base, base + len)` as Normal **non-cacheable** via MPU region 0 and
/// enable the MPU with the default background map (PRIVDEFENA), so all other RAM
/// keeps its default write-back cacheable attribute. This keeps the ethernet DMA
/// buffers coherent while the D-cache stays enabled for everything else.
///
/// `base` must be 32-byte aligned.
fn configure_dma_noncacheable(mpu: &mut MPU, base: u32, len: usize) {
    const MAIR_NORMAL_NC: u32 = 0x44; // outer + inner non-cacheable
    let limit = base + len as u32 - 1;
    unsafe {
        mpu.ctrl.write(0); // disable MPU while reconfiguring
        cortex_m::asm::dsb();
        cortex_m::asm::isb();

        // Attribute index 0 = Normal, non-cacheable.
        let mair0 = mpu.mair[0].read();
        mpu.mair[0].write((mair0 & !0xFF) | MAIR_NORMAL_NC);

        mpu.rnr.write(0);
        // RBAR: BASE[31:5] | SH=00 (non-shareable) | AP=01 (RW, any privilege) | XN=1.
        mpu.rbar.write((base & !0x1F) | (0b01 << 1) | 1);
        // RLAR: LIMIT[31:5] | AttrIndx=0 | EN=1.
        mpu.rlar.write((limit & !0x1F) | 1);

        // Enable MPU, keep the architectural default map as background.
        mpu.ctrl.write((1 << 2) | (1 << 0)); // PRIVDEFENA | ENABLE
        cortex_m::asm::dsb();
        cortex_m::asm::isb();
    }
}

fn config_rifsc() {
    let rifsc = RIFSC;
    rifsc.risc_privcfgr(1).write(|w| w.set_cfg(28, true));
    rifsc.risc_seccfgr(1).write(|w| w.set_cfg(28, true));
    rifsc.rimc_attr(6).write(|w| {
        w.set_mpriv(true);
        w.set_msec(true);
        w.set_mcid(0b01);
    });
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut core_peri = unsafe { cortex_m::Peripherals::steal() };
    core_peri.SCB.invalidate_icache();
    core_peri.SCB.enable_icache();

    static PACKETS: StaticCell<Aligned<PacketQueue<8, 8>>> = StaticCell::new();
    let packets = PACKETS.init(Aligned(PacketQueue::<8, 8>::new()));
    let packet_addr = packets as *const _ as u32;
    info!("Packets Address: {:#010x}", packet_addr);
    configure_dma_noncacheable(
        &mut core_peri.MPU,
        packet_addr,
        core::mem::size_of_val(packets),
    );
    core_peri.SCB.enable_dcache(&mut core_peri.CPUID);

    config_rifsc();

    let p = embassy_stm32::init(rcc_config());

    let mac_addr = [0x00, 0x00, 0xDE, 0xCA, 0xFF, 0xEE];

    let device = Ethernet::new(
        &mut packets.0,
        p.ETH1,
        Irqs,
        p.PF7,
        p.PF10,
        p.PF14,
        p.PF15,
        p.PF12,
        p.PF13,
        p.PF11,
        mac_addr,
        p.ETH_SMA,
        p.PF4,
        p.PG11,
    );

    let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(LOCAL_IP, 24),
        gateway: Some(GATEWAY),
        dns_servers: Vec::new(),
    });

    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    // Fixed seed: this is a local test, no need for entropy.
    let seed = 0x0123_4567_89ab_cdef;
    let (stack, runner) =
        embassy_net::new(device, config, RESOURCES.init(StackResources::new()), seed);

    spawner.spawn(net_task(runner).unwrap());

    stack.wait_link_up().await;
    println!("link up, IP {}", LOCAL_IP);

    const TCP_BUFFER_SIZE: usize = 4096;

    // Large socket buffers -> larger TCP window -> better throughput.
    static RX_BUF: StaticCell<[u8; TCP_BUFFER_SIZE]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; TCP_BUFFER_SIZE]> = StaticCell::new();
    let rx_buf = RX_BUF.init([0; TCP_BUFFER_SIZE]);
    let tx_buf = TX_BUF.init([0; TCP_BUFFER_SIZE]);
    static RX_META: StaticCell<[PacketMetadata; 16]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; 16]> = StaticCell::new();
    let rx_meta = RX_META.init([PacketMetadata::EMPTY; 16]);
    let tx_meta = TX_META.init([PacketMetadata::EMPTY; 16]);
    trace!("trace");
    let mut t = Ticker::every(Duration::from_millis(500));
    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    let a = SocketAddrV4::new(Ipv4Address::new(192, 168, 0, 2), 8000);
    let b = socket.bind(a);
    println!("Bind: {}", b);
    loop {
        t.next().await;
        let e = SocketAddrV4::new(Ipv4Address::BROADCAST, 8000);
        let r = socket.send_to(b"dataaaaaaaaaa", e).await;
        println!("Send? {:?}", r);
        let eth = embassy_stm32::pac::ETH1;
        let mac = eth.ethernet_mac();
        info!("MMCTGFSCCR: {:#010x}", mac.mmc_control().read().0);
        info!("MACRXTXSR: {:#010x}", mac.mac_rx_tx_sr().read().0);
        info!("MACPCSR: {:#010x}", mac.macpcsr().read().0);
        info!("MACLCSR: {:#010x}", mac.maclcsr().read().0);
        info!("MACPHYCSR: {:#010x}", mac.macphycsr().read().0);
        info!("MACDR: {:#010x}", mac.macdr().read().0);
        info!("MACCR: {:#010x}", mac.maccr().read().0);
        info!("MACHWF0R: {:#010x}", mac.machwf0r().read().0);
        info!("MACHWF1R: {:#010x}", mac.machwf1r().read().0);
        info!("MACHWF2R: {:#010x}", mac.machwf2r().read().0);
        info!("MACHWF3R: {:#010x}", mac.machwf3r().read().0);
        info!(
            "Tx packet count good: {:#010x}",
            mac.tx_packet_count_good().read().0
        );
        // Generous idle timeout so a stalled peer can't wedge us forever.
        // socket.set_timeout(Some(Duration::from_secs(20)));
        // let e = socket.write(b"dataaaaaaaaaaaaa").await;
        // println!("Sent? {:?}", e);
        // continue;

        // println!("listening on :{}", PORT);
        // if let Err(e) = socket.accept(PORT).await {
        //     warn!("accept error: {:?}", e);
        //     continue;
        // }
        // println!("client connected");

        // // First byte selects the test mode.
        // let mut cmd = [0u8; 1];
        // if socket.read_exact(&mut cmd).await.is_err() {
        //     warn!("failed to read mode byte");
        //     socket.abort();
        //     let _ = socket.flush().await;
        //     continue;
        // }

        // match cmd[0] {
        //     b'T' => {
        //         println!("mode T: board TX for {} s", DURATION.as_secs());
        //         let n = blast(&mut socket, DURATION).await;
        //         report("board->PC (offered)", n, DURATION);
        //         socket.close(); // FIN so the PC sees EOF
        //     }
        //     b'R' => {
        //         println!("mode R: board RX until peer closes");
        //         let (n, el) = sink(&mut socket).await;
        //         report("PC->board", n, el);
        //     }
        //     b'B' => {
        //         println!("mode B: bidirectional for {} s", DURATION.as_secs());
        //         let (mut reader, mut writer) = socket.split();
        //         let (rx_res, tx_n) = join(sink(&mut reader), blast(&mut writer, DURATION)).await;
        //         report("PC->board", rx_res.0, rx_res.1);
        //         report("board->PC (offered)", tx_n, DURATION);
        //         socket.close();
        //     }
        //     other => {
        //         warn!("unknown mode byte: 0x{:02x}", other);
        //     }
        // }

        // // Drain/settle the close, then drop the socket and re-listen.
        // let _ = socket.flush().await;
        // socket.abort();
        // let _ = socket.flush().await;
        // println!("test done, restarting listener");
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device>) -> ! {
    runner.run().await
}

/// Stream `0xA5` as fast as the socket accepts it, for `dur`. Returns the number
/// of bytes handed to the TCP stack (≈ bytes put on the wire over a long window).
async fn blast<W: Write>(w: &mut W, dur: Duration) -> u64 {
    let buf = [0xA5u8; CHUNK];
    let start = Instant::now();
    let mut total: u64 = 0;
    while Instant::now() - start < dur {
        match w.write(&buf).await {
            Ok(0) => break,
            Ok(n) => total += n as u64,
            Err(_) => break,
        }
    }
    let _ = w.flush().await;
    total
}

/// Drain bytes until EOF/error. Returns (bytes, elapsed-from-first-byte).
async fn sink<R: Read>(r: &mut R) -> (u64, Duration) {
    let mut buf = [0u8; CHUNK];
    let mut total: u64 = 0;
    let mut start: Option<Instant> = None;
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if start.is_none() {
                    start = Some(Instant::now());
                }
                total += n as u64;
            }
            Err(_) => break,
        }
    }
    let elapsed = start
        .map(|s| Instant::now() - s)
        .unwrap_or(Duration::from_ticks(0));
    (total, elapsed)
}

fn report(label: &str, bytes: u64, elapsed: Duration) {
    let us = elapsed.as_micros().max(1);
    // bytes * 8 bits * 100 / microseconds = Mbit/s scaled by 100.
    let mbps_x100 = bytes.saturating_mul(800) / us;
    info!(
        "{}: {} bytes in {} ms -> {}.{:02} Mbit/s",
        label,
        bytes,
        elapsed.as_millis(),
        mbps_x100 / 100,
        mbps_x100 % 100
    );
}
