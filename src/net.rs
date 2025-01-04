use alloc::boxed::Box;
use esp_wifi::esp_now::{EspNow, PeerInfo, BROADCAST_ADDRESS};
use firefly_hal::{Network, NetworkError};
use firefly_types::spi::*;

pub struct Actor<'a> {
    esp_now: EspNow<'a>,
}

impl<'a> Actor<'a> {
    pub fn new(esp_now: EspNow<'a>) -> Self {
        Self { esp_now }
    }

    pub fn handle(&mut self, req: Request) -> Response {
        match self.handle_inner(req) {
            Ok(resp) => resp,
            Err(_) => Response::NetError(0),
        }
    }

    fn handle_inner(&mut self, req: Request) -> Result<Response, NetworkError> {
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
                self.stop()?;
                Response::NetAdvertised
            }
            Request::NetRecv => {
                // match self.recv()? {
                //     Some((addr, msg)) => Response::NetIncoming(addr, &msg),
                //     None => Response::NetNoIncoming,
                // }
                todo!()
            }
            Request::NetSend(addr, data) => {
                self.send(addr, data)?;
                Response::NetSent
            }
            Request::ReadInput => todo!(),
        };
        Ok(resp)
    }
}

pub type Addr = [u8; 6];
type NetworkResult<T> = Result<T, NetworkError>;

impl<'a> Network for Actor<'a> {
    type Addr = [u8; 6];

    fn start(&mut self) -> NetworkResult<()> {
        Ok(())
    }

    fn stop(&mut self) -> NetworkResult<()> {
        Ok(())
    }

    fn local_addr(&self) -> Self::Addr {
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

    fn recv(&mut self) -> NetworkResult<Option<(Self::Addr, Box<[u8]>)>> {
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

    fn send(&mut self, addr: Self::Addr, data: &[u8]) -> NetworkResult<()> {
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
