use crate::retries;
use alloc::{boxed::Box, string::ToString};
use cirque_pinnacle::{Absolute, Touchpad};
use core::convert::Infallible;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::{
    delay::Delay,
    gpio::{Input, Output},
    spi::master::Spi,
    Blocking,
};
use esp_println::println;
use esp_radio::esp_now::*;
use esp_radio::wifi::{PowerSaveMode, WifiController};
use firefly_types::spi::*;
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, IpEndpoint};

type PadSpi<'a> = ExclusiveDevice<Spi<'a, Blocking>, Output<'a>, Delay>;
type RawInput = (Option<(u16, u16)>, u8);

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
    wifi: WifiController<'a>,
    manager: EspNowManager<'a>,
    receiver: EspNowReceiver<'a>,
    buttons: Buttons<'a>,
    socket: tcp::Socket<'a>,
    iface: smoltcp::iface::Interface,
}

impl<'a> Actor<'a> {
    #[must_use]
    pub fn new(
        wifi: WifiController<'a>,
        esp_now: EspNow<'a>,
        pad: Touchpad<PadSpi<'a>, Absolute>,
        buttons: Buttons<'a>,
        socket: tcp::Socket<'a>,
        iface: smoltcp::iface::Interface,
    ) -> Self {
        let (manager, _sender, receiver) = esp_now.split();
        let mut actor = Self {
            pad,
            wifi,
            manager,
            receiver,
            buttons,
            socket,
            iface,
        };
        _ = actor.stop();
        actor
    }

    pub fn handle(&mut self, req: Request) -> RespBuf<'_> {
        match self.handle_inner(req) {
            Ok(resp) => resp,
            Err(err) => {
                println!("error: {err:?}");
                RespBuf::Response(Response::Error(err))
            }
        }
    }

    fn handle_inner<'b>(&mut self, req: Request) -> Result<RespBuf<'b>, &'static str> {
        let response = match req {
            Request::NetStart => {
                self.start()?;
                Response::NetStarted
            }
            Request::NetStop => {
                self.stop()?;
                Response::NetStopped
            }
            Request::NetLocalAddr => {
                let addr = Self::local_addr();
                Response::NetLocalAddr(addr)
            }
            Request::NetAdvertise => {
                Self::advertise();
                Response::NetAdvertised
            }
            Request::NetRecv => match self.recv()? {
                Some((addr, msg)) => return Ok(RespBuf::Incoming(addr, msg)),
                None => Response::NetNoIncoming,
            },
            Request::NetSend(addr, data) => {
                Self::send(addr, data);
                Response::NetSent
            }
            Request::NetSendStatus(addr) => {
                let status = retries::get_status(addr);
                Response::NetSendStatus(status)
            }
            Request::ReadInput => {
                let input = self.read_input()?;
                Response::Input(input.0, input.1)
            }
            Request::WifiConnect(ssid, pass) => {
                self.wifi_connect(ssid, pass)?;
                Response::WifiConnected
            }
            Request::TcpConnect(ip, port) => {
                self.tcp_connect(ip, port)?;
                Response::TcpConnected
            }
            Request::TcpClose => {
                self.tcp_close();
                Response::TcpClosed
            }
        };
        Ok(RespBuf::Response(response))
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
                    Some((touch.x, touch.y))
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
        let res = self.wifi.set_power_saving(PowerSaveMode::None);
        if res.is_err() {
            return Err("failed to exit power saving mode");
        }
        let res = self.wifi.start();
        if res.is_err() {
            return Err("failed to start wifi");
        }
        let res = self.manager.set_channel(6);
        if res.is_err() {
            return Err("failed to set esp-wifi channel");
        }
        // let res = self.manager.set_rate(WifiPhyRate::Rate54m);
        // if res.is_err() {
        //     return Err("failed to set esp-wifi rate");
        // }
        let res = retries::start();
        if let Err(err) = res {
            return Err(convert_error(err));
        }
        Ok(())
    }

    fn wifi_connect(&mut self, ssid: &str, pass: &str) -> NetworkResult<()> {
        use esp_radio::wifi::*;

        let res = self.wifi.set_power_saving(PowerSaveMode::None);
        if res.is_err() {
            return Err("failed to exit power saving mode");
        }
        if !self.wifi.is_started().unwrap_or_default() {
            let res = self.wifi.start();
            if res.is_err() {
                return Err("failed to start wifi");
            }
        }
        let config = ClientConfig::default()
            .with_ssid(ssid.to_string())
            .with_password(pass.to_string());
        let config = ModeConfig::Client(config);
        let res = self.wifi.set_config(&config);
        if res.is_err() {
            return Err("failed to set wifi config");
        }
        let res = self.wifi.connect();
        if res.is_err() {
            return Err("failed to connect to wifi");
        }
        Ok(())
    }

    fn tcp_connect(&mut self, ip: u32, port: u16) -> NetworkResult<()> {
        let cx = self.iface.context();
        let addr = IpAddress::v4(
            (ip >> 24) as u8,
            (ip >> 16) as u8,
            (ip >> 8) as u8,
            ip as u8,
        );
        let remote_endpoint = IpEndpoint::new(addr, port);
        // TODO: Random port (49152 + rand() % 16384)
        let local_endpoint = 49153;
        let res = self.socket.connect(cx, remote_endpoint, local_endpoint);
        if res.is_err() {
            return Err("failed to connect to the TCP endpoint");
        }
        Ok(())
    }

    fn tcp_close(&mut self) {
        self.socket.abort();
    }

    fn stop(&mut self) -> NetworkResult<()> {
        let res = self.wifi.stop();
        if res.is_err() {
            return Err("failed to stop wifi");
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
        let res = retries::stop();
        if let Err(err) = res {
            return Err(convert_error(err));
        }
        Ok(())
    }

    fn local_addr() -> Addr {
        esp_radio::wifi::sta_mac()
    }

    fn advertise() {
        Self::send(BROADCAST_ADDRESS, b"HELLO");
    }

    fn recv(&self) -> NetworkResult<Option<(Addr, Box<[u8]>)>> {
        let Some(packet) = self.receiver.receive() else {
            return Ok(None);
        };

        let known_peer = self.manager.peer_exists(&packet.info.src_address);
        if !known_peer {
            if packet.data() == b"HELLO" {
                let res = self.manager.add_peer(PeerInfo {
                    peer_address: packet.info.src_address,
                    lmk: None,
                    channel: None,
                    encrypt: false,
                    interface: EspNowWifiInterface::Sta,
                });
                if let Err(err) = res {
                    return Err(convert_error(err));
                }
            } else {
                return Ok(None);
            }
        }

        let data = packet.data();
        let data = data.to_vec().into_boxed_slice();
        Ok(Some((packet.info.src_address, data)))
    }

    fn send(addr: Addr, data: &[u8]) {
        retries::send(addr, data);
    }
}

const fn convert_error(value: esp_radio::esp_now::EspNowError) -> &'static str {
    use esp_radio::esp_now::EspNowError;
    match value {
        EspNowError::Error(error) => match error {
            esp_radio::esp_now::Error::NotInitialized => "esp-now: not initialized",
            esp_radio::esp_now::Error::InvalidArgument => "esp-now: invalid argument",
            esp_radio::esp_now::Error::OutOfMemory => {
                "esp-now: insufficient memory to complete the operation"
            }
            esp_radio::esp_now::Error::PeerListFull => "esp-now: peer list is full",
            esp_radio::esp_now::Error::NotFound => "esp-now: peer is not found",
            esp_radio::esp_now::Error::Internal => "esp-now: internal error",
            esp_radio::esp_now::Error::PeerExists => "esp-now: peer already exists",
            esp_radio::esp_now::Error::InterfaceMismatch => "esp-now: interface mismatch",
            esp_radio::esp_now::Error::Other(_) => "esp-now: unknown error",
        },
        EspNowError::SendFailed => "esp-now: failed to send message",
        EspNowError::DuplicateInstance => "esp-now: duplicate instance",
        EspNowError::Initialization(error) => match error {
            esp_radio::wifi::WifiError::NotInitialized => "wifi init: not initialized",
            esp_radio::wifi::WifiError::InternalError(_) => "wifi init: internal error",
            esp_radio::wifi::WifiError::Disconnected => "wifi init: disconnected",
            esp_radio::wifi::WifiError::UnknownWifiMode => "wifi init: unknown WiFi mode",
            esp_radio::wifi::WifiError::Unsupported => "wifi init: unsupported",
            _ => "wifi init: unknown error",
        },
    }
}

const fn convert_error2(
    value: embedded_hal_bus::spi::DeviceError<esp_hal::spi::Error, Infallible>,
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
