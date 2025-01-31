use alloc::boxed::Box;
use cirque_pinnacle::{Absolute, Touchpad};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::{
    delay::Delay,
    gpio::{Input, Output},
    spi::master::Spi,
    Blocking,
};
use esp_println::println;
use esp_wifi::{config::PowerSaveMode, esp_now::*};
use firefly_types::spi::*;

type PadSpi<'a> = ExclusiveDevice<Spi<'a, Blocking>, Output<'a>, Delay>;
type RawInput = (Option<(i16, i16)>, u8);

pub struct Buttons<'a> {
    pub s: Input<'a>,
    pub e: Input<'a>,
    pub w: Input<'a>,
    pub n: Input<'a>,
    pub menu: Input<'a>,
}

pub enum RespBuf<'a> {
    Response(Response<'a>),
    Incoming([u8; 6], Box<[u8]>),
}

pub struct Actor<'a> {
    pad: Touchpad<PadSpi<'a>, Absolute>,
    manager: EspNowManager<'a>,
    sender: EspNowSender<'a>,
    receiver: EspNowReceiver<'a>,
    buttons: Buttons<'a>,
}

impl<'a> Actor<'a> {
    pub fn new(
        esp_now: EspNow<'a>,
        pad: Touchpad<PadSpi<'a>, Absolute>,
        buttons: Buttons<'a>,
    ) -> Self {
        let (manager, sender, receiver) = esp_now.split();
        // begin with networking disabled
        _ = manager.set_power_saving(PowerSaveMode::Maximum);
        Self {
            manager,
            sender,
            receiver,
            pad,
            buttons,
        }
    }

    pub fn handle(&mut self, req: Request) -> RespBuf {
        match self.handle_inner(req) {
            Ok(resp) => resp,
            Err(err) => {
                println!("error: {err:?}");
                RespBuf::Response(Response::Error(err))
            }
        }
    }

    fn handle_inner<'b>(&mut self, req: Request) -> Result<RespBuf<'b>, &'static str> {
        let resp = match req {
            Request::NetStart => {
                self.start()?;
                Response::NetStarted
            }
            Request::NetStop => {
                self.stop()?;
                Response::NetStopped
            }
            Request::NetLocalAddr => {
                let addr = self.local_addr();
                Response::NetLocalAddr(addr)
            }
            Request::NetAdvertise => {
                self.advertise()?;
                Response::NetAdvertised
            }
            Request::NetRecv => match self.recv()? {
                Some((addr, msg)) => return Ok(RespBuf::Incoming(addr, msg)),
                None => Response::NetNoIncoming,
            },
            Request::NetSend(addr, data) => {
                self.send(addr, data)?;
                Response::NetSent
            }
            Request::ReadInput => {
                let input = self.read_input()?;
                Response::Input(input.0, input.1)
            }
        };
        Ok(RespBuf::Response(resp))
    }

    fn read_input(&mut self) -> Result<RawInput, &'static str> {
        let buttons = u8::from(self.buttons.s.is_high())
            | u8::from(self.buttons.e.is_high()) << 1
            | u8::from(self.buttons.w.is_high()) << 2
            | u8::from(self.buttons.n.is_high()) << 3
            | u8::from(self.buttons.menu.is_high()) << 4;
        match self.pad.read_absolute() {
            Ok(touch) => {
                let pad = if touch.touched() {
                    let x = (1000. * touch.x_f32()) as i16;
                    let y = (1000. * touch.y_f32()) as i16;
                    Some((x, y))
                } else {
                    None
                };
                Ok((pad, buttons))
            }
            Err(err) => Err(convert_error2(err)),
        }
    }
}

pub type Addr = [u8; 6];
type NetworkResult<T> = Result<T, &'static str>;

impl Actor<'_> {
    fn start(&mut self) -> NetworkResult<()> {
        let res = self.manager.set_power_saving(PowerSaveMode::None);
        if res.is_err() {
            return Err("failed to exit power saving mode");
        }
        Ok(())
    }

    fn stop(&mut self) -> NetworkResult<()> {
        let res = self.manager.set_power_saving(PowerSaveMode::Maximum);
        if res.is_err() {
            return Err("failed to enter power saving mode");
        }
        loop {
            let Ok(peer) = self.manager.fetch_peer(true) else {
                break;
            };
            let res = self.manager.remove_peer(&peer.peer_address);
            if res.is_err() {
                return Err("peer not found, cannot remove");
            }
        }
        Ok(())
    }

    fn local_addr(&self) -> Addr {
        let mut addr = [0u8; 6];
        esp_wifi::wifi::sta_mac(&mut addr);
        addr
    }

    fn advertise(&mut self) -> NetworkResult<()> {
        let data = b"HELLO";
        let waiter = match self.sender.send(&BROADCAST_ADDRESS, &data[..]) {
            Ok(waiter) => waiter,
            Err(err) => return Err(convert_error(err)),
        };
        let res = waiter.wait();
        if let Err(err) = res {
            return Err(convert_error(err));
        }
        Ok(())
    }

    fn recv(&mut self) -> NetworkResult<Option<(Addr, Box<[u8]>)>> {
        let Some(packet) = self.receiver.receive() else {
            return Ok(None);
        };

        if !self.manager.peer_exists(&packet.info.src_address) {
            let res = self.manager.add_peer(PeerInfo {
                peer_address: packet.info.src_address,
                lmk: None,
                channel: None,
                encrypt: false,
            });
            if let Err(err) = res {
                return Err(convert_error(err));
            }
        }

        let data = packet.data();
        let data = data.to_vec().into_boxed_slice();
        Ok(Some((packet.info.src_address, data)))
    }

    fn send(&mut self, addr: Addr, data: &[u8]) -> NetworkResult<()> {
        let res = self.sender.send(&addr, data);
        if let Err(err) = res {
            return Err(convert_error(err));
        };
        // TODO: figure out retrieving errors from waiter later.
        // let res = waiter.wait();
        // if let Err(err) = res {
        //     return Err(convert_error(err));
        // }
        Ok(())
    }
}

fn convert_error(value: esp_wifi::esp_now::EspNowError) -> &'static str {
    use esp_wifi::esp_now::EspNowError;
    match value {
        EspNowError::Error(error) => match error {
            esp_wifi::esp_now::Error::NotInitialized => "esp-now: not initialized",
            esp_wifi::esp_now::Error::InvalidArgument => "esp-now: invalid argument",
            esp_wifi::esp_now::Error::OutOfMemory => {
                "esp-now: insufficient memory to complete the operation"
            }
            esp_wifi::esp_now::Error::PeerListFull => "esp-now: peer list is full",
            esp_wifi::esp_now::Error::NotFound => "esp-now: peer is not found",
            esp_wifi::esp_now::Error::InternalError => "esp-now: internal error",
            esp_wifi::esp_now::Error::PeerExists => "esp-now: peer already exists",
            esp_wifi::esp_now::Error::InterfaceError => "esp-now: interface error",
            esp_wifi::esp_now::Error::Other(_) => "esp-now: unknown error",
        },
        EspNowError::SendFailed => "esp-now: failed to send message",
        EspNowError::DuplicateInstance => "esp-now: duplicate instance",
        EspNowError::Initialization(error) => match error {
            esp_wifi::wifi::WifiError::NotInitialized => "wifi init: not initialized",
            esp_wifi::wifi::WifiError::InternalError(_) => "wifi init: internal error",
            esp_wifi::wifi::WifiError::Disconnected => "wifi init: disconnected",
            esp_wifi::wifi::WifiError::UnknownWifiMode => "wifi init: unknown WiFi mode",
            esp_wifi::wifi::WifiError::Unsupported => "wifi init: unsupported",
        },
    }
}

fn convert_error2(
    value: embedded_hal_bus::spi::DeviceError<esp_hal::spi::Error, core::convert::Infallible>,
) -> &'static str {
    use esp_hal::dma::DmaError;
    match value {
        embedded_hal_bus::spi::DeviceError::Spi(err) => match err {
            esp_hal::spi::Error::DmaError(err) => match err {
                DmaError::InvalidAlignment(_) => "spi: dmi: invalid alignment",
                DmaError::OutOfDescriptors => "spi: dmi: out of descriptors",
                DmaError::DescriptorError => "spi: dmi: descriptor error",
                DmaError::Overflow => "spi: dmi: overflow",
                DmaError::BufferTooSmall => "spi: dmi: buffer too small",
                DmaError::UnsupportedMemoryRegion => "spi: dmi: unsupported_memory_region",
                DmaError::InvalidChunkSize => "spi: dmi: invalid chunk size",
                DmaError::Late => "spi: dmi: late",
            },
            esp_hal::spi::Error::MaxDmaTransferSizeExceeded => {
                "the maximum DMA transfer size was exceeded"
            }
            esp_hal::spi::Error::FifoSizeExeeded => {
                "the FIFO size was exceeded during SPI communication"
            }
            esp_hal::spi::Error::Unsupported => "spi: the operation is unsupported",
            esp_hal::spi::Error::Unknown => {
                "spi: an unknown error occurred during SPI communication"
            }
            _ => "spi: unknown error",
        },
        embedded_hal_bus::spi::DeviceError::Cs(_) => "spi: asserting or deasserting CS failed",
    }
}
