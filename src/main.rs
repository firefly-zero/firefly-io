#![no_std]
#![no_main]

extern crate alloc;

use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_io::Read;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig},
    main,
    rng::Rng,
    time::Rate,
    timer::timg::TimerGroup,
    uart::Uart,
    Blocking,
};
use esp_println::println;
use firefly_io::*;
use firefly_types::{spi::*, Encode};

#[main]
fn main() -> ! {
    esp_alloc::heap_allocator!(size: 120 * 1024);
    println!("creating device config...");
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    println!("initializing peripherals...");
    let peripherals = esp_hal::init(config);

    println!("configuring esp-now...");
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let inited = esp_wifi::init(
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    )
    .unwrap();
    let (mut wifi, interfaces) = esp_wifi::wifi::new(&inited, peripherals.WIFI).unwrap();
    wifi.set_mode(esp_wifi::wifi::WifiMode::Sta).unwrap();
    let esp_now = interfaces.esp_now;

    println!("configuring touch pad...");
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
            .unwrap()
            .with_sck(sclk)
            .with_mosi(mosi)
            .with_miso(miso);
        let spi_device = ExclusiveDevice::new(spi, cs, delay).unwrap();
        let mode = cirque_pinnacle::Absolute::default();
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

    let mut actor = Actor::new(wifi, esp_now, pad, buttons);

    println!("configuring main SPI...");
    let mut uart_main = {
        let miso = peripherals.GPIO21;
        let mosi = peripherals.GPIO45;
        let config = esp_hal::uart::Config::default();
        Uart::new(peripherals.UART1, config)
            .unwrap()
            .with_rx(miso)
            .with_tx(mosi)
    };

    println!("listening...");
    let buf = &mut [0u8; 300];
    loop {
        // read request size
        uart_main.read(&mut buf[..1]).unwrap();
        let size = usize::from(buf[0]);

        // read request payload
        uart_main.read_exact(&mut buf[..size]).unwrap();
        let req = Request::decode(&buf[..size]).unwrap();

        match actor.handle(req) {
            RespBuf::Response(resp) => send_resp(&mut uart_main, buf, resp),
            RespBuf::Incoming(addr, msg) => {
                let resp = Response::NetIncoming(addr, &msg);
                send_resp(&mut uart_main, buf, resp);
            }
        };
    }
}

fn send_resp(uart: &mut Uart<'_, Blocking>, buf: &mut [u8], resp: Response<'_>) {
    let (head, tail) = buf.split_at_mut(1);
    let buf = resp.encode_buf(tail).unwrap();
    let Ok(size) = u8::try_from(buf.len()) else {
        // The payload is too big. The only Response that can, in theory, be big
        // is NetIncoming. So we can assume that it's a message receiving error.
        // But just in case, we want to be sure not to fall into an infinite recursion.
        if !matches!(resp, Response::Error(_)) {
            let resp = Response::Error("response is too big");
            send_resp(uart, buf, resp);
        }
        return;
    };
    head[0] = size;
    uart.write(head).unwrap();
    uart.write(buf).unwrap();
}
