use esp_wifi::esp_now::{EspNow, PeerInfo, BROADCAST_ADDRESS};
use firefly_hal::{Network, NetworkError};

pub struct Actor<'a> {
    esp_now: EspNow<'a>,
}

impl<'a> Actor<'a> {
    pub fn new(esp_now: EspNow<'a>) -> Self {
        Self { esp_now }
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
        let data = heapless::Vec::<u8, 64>::from_slice(b"HELLO").unwrap();
        let waiter = self.esp_now.send(&BROADCAST_ADDRESS, &data)?;
        waiter.wait()?;
        Ok(())
    }

    fn recv(&mut self) -> NetworkResult<Option<(Self::Addr, heapless::Vec<u8, 64>)>> {
        let Some(packet) = self.esp_now.receive() else {
            return Ok(None);
        };

        if !self.esp_now.peer_exists(&packet.info.src_address) {
            self.esp_now.add_peer(PeerInfo {
                peer_address: packet.info.src_address,
                lmk: None,
                channel: None,
                encrypt: false,
            })?;
        }

        let data = packet.data();
        let data = heapless::Vec::<u8, 64>::from_slice(data).unwrap();
        Ok(Some((packet.info.src_address, data)))
    }

    fn send(&mut self, addr: Self::Addr, data: &[u8]) -> NetworkResult<()> {
        let waiter = self.esp_now.send(&addr, data)?;
        waiter.wait()?;
        Ok(())
    }
}
