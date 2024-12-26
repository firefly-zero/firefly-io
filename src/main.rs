#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::{delay::Delay, prelude::*, rng::Rng, timer::timg::TimerGroup};
use esp_println::println;
use firefly_net::Actor;

#[entry]
fn main() -> ! {
    esp_alloc::heap_allocator!(300 * 1024);
    run();
    println!("end");
    let delay = Delay::new();
    loop {
        delay.delay(500.millis());
    }
}

fn run() {
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
    let net = Actor::new(esp_now);
}
