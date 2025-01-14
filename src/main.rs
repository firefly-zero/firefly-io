#![no_std]
#![no_main]

extern crate alloc;

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::{Input, Level, Output},
    prelude::*,
    rng::Rng,
    spi::SpiMode,
    timer::timg::TimerGroup,
    uart::Uart,
    Blocking,
};
use esp_println::println;
use firefly_io::*;
use firefly_types::{spi::*, Encode};

#[entry]
fn main() -> ! {
    esp_alloc::heap_allocator!(120 * 1024);
    println!("creating device config...");
    let mut config = esp_hal::Config::default();
    config.cpu_clock = CpuClock::max();
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
    let esp_now = esp_wifi::esp_now::EspNow::new(&inited, peripherals.WIFI).unwrap();

    println!("configuring touch pad...");
    let pad = {
        let delay = Delay::new();
        let sclk = peripherals.GPIO4;
        let miso = peripherals.GPIO5;
        let mosi = peripherals.GPIO15;
        let cs = peripherals.GPIO6;
        // let dr = peripherals.GPIO7;

        let cs = Output::new(cs, Level::High);
        let spi = esp_hal::spi::master::Spi::new_with_config(
            peripherals.SPI3,
            esp_hal::spi::master::Config {
                frequency: 400u32.kHz(),
                mode: SpiMode::Mode1,
                ..esp_hal::spi::master::Config::default()
            },
        )
        .with_sck(sclk)
        .with_mosi(mosi)
        .with_miso(miso);
        let spi_device = ExclusiveDevice::new(spi, cs, delay).unwrap();
        let mode = cirque_pinnacle::Absolute::default();
        mode.init(spi_device).unwrap()
    };

    let buttons = Buttons {
        s: Input::new(peripherals.GPIO9, esp_hal::gpio::Pull::Up),
        e: Input::new(peripherals.GPIO46, esp_hal::gpio::Pull::Up),
        w: Input::new(peripherals.GPIO11, esp_hal::gpio::Pull::Up),
        n: Input::new(peripherals.GPIO10, esp_hal::gpio::Pull::Up),
        menu: Input::new(peripherals.GPIO3, esp_hal::gpio::Pull::Up),
    };

    let mut actor = Actor::new(esp_now, pad, buttons);

    println!("configuring main SPI...");
    let mut uart_main = {
        let miso = peripherals.GPIO21;
        let mosi = peripherals.GPIO45;
        Uart::new(peripherals.UART1, miso, mosi).unwrap()
    };

    println!("listening...");
    let buf = &mut [0u8; 300];
    loop {
        // read request size
        uart_main.read_bytes(&mut buf[..1]).unwrap();
        let size = usize::from(buf[0]);
        println!("reading {size} bytes...");

        // read request payload
        uart_main.read_bytes(&mut buf[..size]).unwrap();
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
        // The payload is too big.  The only Response that can, in theory, be big
        // is NetIncoming. So we can assume that it's a message receiving error.
        // But just in case, we want to be sure not to fall into an infinite recursion.
        if !matches!(resp, Response::NetError(_)) {
            let resp = Response::NetError(NetworkError::RecvError.into());
            send_resp(uart, buf, resp);
        }
        return;
    };
    println!("sending {size} bytes...");
    head[0] = size;
    uart.write_bytes(head).unwrap();
    uart.write_bytes(buf).unwrap();
}
