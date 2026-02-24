use crate::*;
use alloc::vec;
use anyhow::{Context, Result};
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_io::Read;
use esp_hal::{
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig},
    peripherals::Peripherals,
    time::Rate,
    timer::timg::TimerGroup,
    uart::Uart,
};
use esp_println::println;
use firefly_types::{spi::*, Encode};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};

pub fn run_v1(peripherals: Peripherals) -> Result<()> {
    println!("starting RTOS scheduler...");
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    println!("configuring WiFi...");
    let inited = esp_radio::init().context("init wifi")?;
    let config = esp_radio::wifi::Config::default();
    let (mut wifi, interfaces) = esp_radio::wifi::new(&inited, peripherals.WIFI, config)
        .context("create wifi controller")?;
    wifi.set_mode(esp_radio::wifi::WifiMode::Sta)
        .context("enter sta mode")?;
    let esp_now = interfaces.esp_now;

    println!("configuring touchpad...");
    let pad = {
        let delay = Delay::new();
        let sclk = peripherals.GPIO4;
        let miso = peripherals.GPIO5;
        let mosi = peripherals.GPIO15;
        let cs = peripherals.GPIO6;
        // let dr = peripherals.GPIO7;

        let cs = Output::new(cs, Level::High, OutputConfig::default());
        let config = esp_hal::spi::master::Config::default()
            .with_frequency(Rate::from_khz(400))
            .with_mode(esp_hal::spi::Mode::_1);
        let spi = esp_hal::spi::master::Spi::new(peripherals.SPI3, config)
            .context("init spi")?
            .with_sck(sclk)
            .with_mosi(mosi)
            .with_miso(miso);
        let spi_device = ExclusiveDevice::new(spi, cs, delay).context("access spi")?;
        let mode = cirque_pinnacle::Absolute::default();
        // TODO(@orsinium): don't unwrap
        mode.init(spi_device).unwrap()
    };

    let up = InputConfig::default().with_pull(esp_hal::gpio::Pull::Up);
    let buttons = Buttons {
        s: Input::new(peripherals.GPIO9, up),
        e: Input::new(peripherals.GPIO46, up),
        w: Input::new(peripherals.GPIO11, up),
        n: Input::new(peripherals.GPIO10, up),
        menu: Input::new(peripherals.GPIO3, up),
    };

    println!("configuring TCP/IP stack...");
    let mut device = interfaces.sta;
    let addr = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let config = smoltcp::iface::Config::new(addr.into());
    let now = smoltcp::time::Instant::from_micros(1);
    let mut iface = smoltcp::iface::Interface::new(config, &mut device, now);
    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs
            .push(IpCidr::new(IpAddress::v4(192, 168, 69, 1), 24))
            .unwrap();
    });
    iface
        .routes_mut()
        .add_default_ipv4_route(core::net::Ipv4Addr::new(192, 168, 69, 100))
        .unwrap();
    let rbuf = tcp::SocketBuffer::new(vec![0; 1024]);
    let tbuf = tcp::SocketBuffer::new(vec![0; 1024]);
    let tcp_socket = tcp::Socket::new(rbuf, tbuf);

    let mut actor = Actor::new(wifi, esp_now, pad, buttons, tcp_socket, iface);

    println!("configuring main SPI...");
    let mut uart_main = {
        let miso = peripherals.GPIO21;
        let mosi = peripherals.GPIO45;
        let config = esp_hal::uart::Config::default().with_baudrate(921_600);
        Uart::new(peripherals.UART1, config)
            .context("init uart")?
            .with_rx(miso)
            .with_tx(mosi)
    };

    println!("listening...");
    let buf = &mut [0u8; 300];
    loop {
        // read request size
        uart_main.read(&mut buf[..1]).context("read request size")?;
        let size = usize::from(buf[0]);

        // read request payload
        // TODO(@orsinium): don't unwrap
        uart_main.read_exact(&mut buf[..size]).unwrap();
        let req = Request::decode(&buf[..size]).context("decode request")?;

        match actor.handle(req) {
            RespBuf::Response(resp) => {
                send_resp(&mut uart_main, buf, resp)?;
            }
            RespBuf::Incoming(addr, msg) => {
                let resp = Response::NetIncoming(addr, &msg);
                send_resp(&mut uart_main, buf, resp)?;
            }
            RespBuf::Scan(ssids) => {
                let ssids = [
                    ssids[0].as_str(),
                    ssids[1].as_str(),
                    ssids[2].as_str(),
                    ssids[3].as_str(),
                    ssids[4].as_str(),
                    ssids[5].as_str(),
                ];
                let resp = Response::WifiScan(ssids);
                send_resp(&mut uart_main, buf, resp)?;
            }
        }
    }
}
