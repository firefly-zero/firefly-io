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
use esp_wifi::esp_now::{EspNow, PeerInfo, BROADCAST_ADDRESS};
use firefly_types::spi::*;

type PadSpi<'a> = ExclusiveDevice<Spi<'a, Blocking>, Output<'a>, Delay>;

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
    esp_now: EspNow<'a>,
    buttons: Buttons<'a>,
}

impl<'a> Actor<'a> {
    pub fn new(
        esp_now: EspNow<'a>,
        pad: Touchpad<PadSpi<'a>, Absolute>,
        buttons: Buttons<'a>,
    ) -> Self {
        Self {
            esp_now,
            pad,
            buttons,
        }
    }

    pub fn handle(&mut self, req: Request) -> RespBuf {
        match self.handle_inner(req) {
            Ok(resp) => resp,
            Err(err) => {
                println!("network error: {err:?}");
                RespBuf::Response(Response::NetError(err.into()))
            }
        }
    }

    fn handle_inner<'b>(&mut self, req: Request) -> Result<RespBuf<'b>, NetworkError> {
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
            Request::ReadInput => match self.read_input() {
                Some(input) => Response::Input(input.0, input.1),
                None => Response::PadError,
            },
        };
        Ok(RespBuf::Response(resp))
    }

    fn read_input(&mut self) -> Option<(Option<(i16, i16)>, u8)> {
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
                Some((pad, buttons))
            }
            Err(err) => {
                let err: NetworkError = err.into();
                println!("touchpad error: {err:?}");
                None
            }
        }
    }
}

pub type Addr = [u8; 6];
type NetworkResult<T> = Result<T, NetworkError>;

impl<'a> Actor<'a> {
    fn start(&mut self) -> NetworkResult<()> {
        Ok(())
    }

    fn stop(&mut self) -> NetworkResult<()> {
        Ok(())
    }

    fn local_addr(&self) -> Addr {
        let mut addr = [0u8; 6];
        esp_wifi::wifi::sta_mac(&mut addr);
        addr
    }

    fn advertise(&mut self) -> NetworkResult<()> {
        let data = b"HELLO";
        let waiter = match self.esp_now.send(&BROADCAST_ADDRESS, &data[..]) {
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
        let Some(packet) = self.esp_now.receive() else {
            return Ok(None);
        };

        if !self.esp_now.peer_exists(&packet.info.src_address) {
            let res = self.esp_now.add_peer(PeerInfo {
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
        let waiter = match self.esp_now.send(&addr, data) {
            Ok(waiter) => waiter,
            Err(err) => return Err(convert_error(err)),
        };
        let res = waiter.wait();
        if let Err(err) = res {
            return Err(convert_error(err));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum NetworkError {
    NotInitialized,
    AlreadyInitialized,
    UnknownPeer,
    CannotBind,
    PeerListFull,
    RecvError,
    SendError,
    NetThreadDeallocated,
    OutMessageTooBig,
    UnexpectedResp,
    Spi(esp_hal::spi::Error),
    Error(&'static str),
    Other(u32),
}

impl From<embedded_hal_bus::spi::DeviceError<esp_hal::spi::Error, core::convert::Infallible>>
    for NetworkError
{
    fn from(
        value: embedded_hal_bus::spi::DeviceError<esp_hal::spi::Error, core::convert::Infallible>,
    ) -> Self {
        match value {
            embedded_hal_bus::spi::DeviceError::Spi(err) => Self::Spi(err),
            embedded_hal_bus::spi::DeviceError::Cs(_) => Self::Error("CS error"),
        }
    }
}

impl From<NetworkError> for u32 {
    fn from(value: NetworkError) -> Self {
        match value {
            NetworkError::NotInitialized => 0,
            NetworkError::AlreadyInitialized => 1,
            NetworkError::UnknownPeer => 2,
            NetworkError::CannotBind => 3,
            NetworkError::PeerListFull => 4,
            NetworkError::RecvError => 5,
            NetworkError::SendError => 6,
            NetworkError::NetThreadDeallocated => 7,
            NetworkError::OutMessageTooBig => 8,
            NetworkError::UnexpectedResp => 9,
            #[cfg(target_os = "none")]
            NetworkError::Spi(_) => 10,
            NetworkError::Error(_) => 11,
            NetworkError::Other(x) => 100 + x,
        }
    }
}

fn convert_error(value: esp_wifi::esp_now::EspNowError) -> NetworkError {
    use esp_wifi::esp_now::EspNowError;
    match value {
        EspNowError::Error(error) => match error {
            esp_wifi::esp_now::Error::NotInitialized => NetworkError::NotInitialized,
            esp_wifi::esp_now::Error::InvalidArgument => NetworkError::Error("invalid argument"),
            esp_wifi::esp_now::Error::OutOfMemory => NetworkError::Error("out of memory"),
            esp_wifi::esp_now::Error::PeerListFull => NetworkError::PeerListFull,
            esp_wifi::esp_now::Error::NotFound => NetworkError::Error("not found"),
            esp_wifi::esp_now::Error::InternalError => NetworkError::Error("internal error"),
            esp_wifi::esp_now::Error::PeerExists => NetworkError::Error("peer exists"),
            esp_wifi::esp_now::Error::InterfaceError => NetworkError::Error("interface error"),
            esp_wifi::esp_now::Error::Other(error) => NetworkError::Other(error),
        },
        EspNowError::SendFailed => NetworkError::SendError,
        EspNowError::DuplicateInstance => NetworkError::AlreadyInitialized,
        EspNowError::Initialization(error) => match error {
            esp_wifi::wifi::WifiError::NotInitialized => NetworkError::NotInitialized,
            esp_wifi::wifi::WifiError::InternalError(_) => NetworkError::Error("internal error"),
            esp_wifi::wifi::WifiError::Disconnected => NetworkError::NetThreadDeallocated,
            esp_wifi::wifi::WifiError::UnknownWifiMode => NetworkError::Error("unknown wifi mode"),
            esp_wifi::wifi::WifiError::Unsupported => NetworkError::Error("unsupported"),
        },
    }
}
