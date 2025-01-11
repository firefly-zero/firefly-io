#![no_std]
#![no_main]

extern crate alloc;

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    dma::{Dma, DmaPriority},
    dma_buffers,
    gpio::{Level, Output},
    prelude::*,
    rng::Rng,
    spi::SpiMode,
    timer::timg::TimerGroup,
};
use esp_println::println;
use firefly_hal::NetworkError;
use firefly_io::*;
use firefly_types::{spi::*, Encode};

#[entry]
fn main() -> ! {
    esp_alloc::heap_allocator!(300 * 1024);
    println!("creating device config...");
    let mut config = esp_hal::Config::default();
    config.cpu_clock = CpuClock::max();
    println!("initializing peripherals...");
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let inited = esp_wifi::init(
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    )
    .unwrap();
    let esp_now = esp_wifi::esp_now::EspNow::new(&inited, peripherals.WIFI).unwrap();

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

    let mut actor = Actor::new(esp_now, pad);

    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = dma_buffers!(32000);
    let mut spi_main = {
        let dma = Dma::new(peripherals.DMA);
        let dma_channel = dma.channel0;
        let sclk = peripherals.GPIO14;
        let miso = peripherals.GPIO21;
        let mosi = peripherals.GPIO45;

        esp_hal::spi::slave::Spi::new(peripherals.SPI2, SpiMode::Mode0)
            .with_sck(sclk)
            .with_mosi(mosi)
            .with_miso(miso)
            .with_dma(
                dma_channel.configure(false, DmaPriority::Priority0),
                rx_descriptors,
                tx_descriptors,
            )
    };

    let receive = rx_buffer;
    let send = tx_buffer;

    println!("listening...");
    loop {
        // read request size
        let mut buf = &mut receive[..1];
        let waiter = spi_main.read(&mut buf).unwrap();
        waiter.wait().unwrap();
        let size = usize::from(buf[0]);

        // read request payload
        let mut buf = &mut receive[size..];
        let waiter = spi_main.read(&mut buf).unwrap();
        waiter.wait().unwrap();
        let req = Request::decode(buf).unwrap();

        match actor.handle(req) {
            RespBuf::Response(resp) => send_resp(&mut spi_main, send, resp),
            RespBuf::Incoming(addr, msg) => {
                let resp = Response::NetIncoming(addr, &msg);
                send_resp(&mut spi_main, send, resp);
            }
        };
    }
}

fn send_resp(
    spi: &mut esp_hal::spi::slave::dma::SpiDma<'_, esp_hal::Blocking>,
    send: &mut [u8; 32000],
    resp: Response<'_>,
) {
    let (head, tail) = send.split_at_mut(1);
    let buf = resp.encode_buf(tail).unwrap();
    let Ok(size) = u8::try_from(buf.len()) else {
        // The payload is too big.  The only Response that can, in theory, be big
        // is NetIncoming. So we can assume that it's a message receiving error.
        // But just in case, we want to be sure not to fall into an infinite recursion.
        if !matches!(resp, Response::NetError(_)) {
            let resp = Response::NetError(NetworkError::RecvError.into());
            send_resp(spi, send, resp);
        }
        return;
    };
    head[0] = size;
    let waiter = spi.write(&head).unwrap();
    waiter.wait().unwrap();
    let waiter = spi.write(&buf).unwrap();
    waiter.wait().unwrap();
}
