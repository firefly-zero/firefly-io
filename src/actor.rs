use crate::{retries, wifi::WifiManager, ErrPrinter};
use alloc::{boxed::Box, string::String};
use anyhow::{bail, Result};
use cirque_pinnacle::{Absolute, Touchpad};
use core::convert::Infallible;
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_storage::Storage;
use esp_bootloader_esp_idf::{
    ota::Ota,
    partitions::{read_partition_table, AppPartitionSubType, DataPartitionSubType, PartitionType},
};
use esp_hal::{
    delay::Delay,
    gpio::{Input, Output},
    spi::master::Spi,
    Blocking,
};
use esp_println::println;
use esp_radio::esp_now::*;
use esp_storage::FlashStorage;
use firefly_types::spi::{Request, Response};

type PadSpi<'a> = ExclusiveDevice<Spi<'a, Blocking>, Output<'a>, Delay>;
type RawInput = (Option<(u16, u16)>, u8);
pub type Addr = [u8; 6];

pub struct Buttons<'a> {
    pub s: Input<'a>,
    pub e: Input<'a>,
    pub w: Input<'a>,
    pub n: Input<'a>,
    pub menu: Input<'a>,
}

/// An extension for [`Response`] that owns fields that the original struct borrows.
pub enum RespBuf<'a> {
    Response(Response<'a>),
    Incoming([u8; 6], Box<[u8]>),
    Scan([String; 6]),
    TcpChunk(Box<[u8]>),
    Err(String),
}

pub struct Actor<'a> {
    pad: Touchpad<PadSpi<'a>, Absolute>,
    manager: EspNowManager<'a>,
    receiver: EspNowReceiver<'a>,
    buttons: Buttons<'a>,
    wifi: WifiManager<'a>,
    flash: FlashStorage<'a>,
}

impl<'a> Actor<'a> {
    #[must_use]
    pub fn new(
        esp_now: EspNow<'a>,
        pad: Touchpad<PadSpi<'a>, Absolute>,
        buttons: Buttons<'a>,
        wifi: WifiManager<'a>,
        flash: FlashStorage<'a>,
    ) -> Self {
        let (manager, _sender, receiver) = esp_now.split();
        let mut actor = Self {
            pad,
            manager,
            receiver,
            buttons,
            wifi,
            flash,
        };
        _ = actor.stop();
        actor
    }

    pub fn handle(&mut self, req: Request) -> RespBuf<'_> {
        match self.handle_inner(req) {
            Ok(resp) => resp,
            Err(err) => {
                let err = alloc::format!("{}", ErrPrinter(err));
                println!("error: {err:?}");
                RespBuf::Err(err)
            }
        }
    }

    fn handle_inner<'b>(&mut self, req: Request) -> Result<RespBuf<'b>> {
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
            Request::FirmwareInfo => {
                let version = get_firmware_version();
                let partition = self.get_partition()?;
                Response::FirmwareInfo { version, partition }
            }
            Request::WifiScan => {
                let ssids = self.wifi.scan()?;
                return Ok(RespBuf::Scan(ssids));
            }
            Request::WifiConnect(ssid, pass) => {
                self.wifi.connect(ssid, pass)?;
                Response::WifiConnected
            }
            Request::WifiStatus => {
                let status = self.wifi.status()?;
                Response::WifiStatus(status)
            }
            Request::WifiDisconnect => {
                self.wifi.disconnect()?;
                Response::WifiDisconnected
            }
            Request::TcpConnect(ip, port) => {
                self.wifi.tcp_connect(ip, port)?;
                Response::TcpConnected
            }
            Request::TcpStatus => {
                let status = self.wifi.tcp_status();
                Response::TcpStatus(status)
            }
            Request::TcpSend(data) => {
                self.wifi.tcp_send(data)?;
                Response::TcpSent
            }
            Request::TcpRecv => {
                let data = self.wifi.tcp_recv()?;
                return Ok(RespBuf::TcpChunk(data));
            }
            Request::TcpClose => {
                self.wifi.tcp_close();
                Response::TcpClosed
            }
            Request::FlashWrite(offset, data) => {
                _ = self.flash.write(offset, data);
                Response::FlashWritten
            }
            Request::PartitionSwitch(part) => {
                self.switch_partition(part)?;
                Response::PartitionSwitched
            }
        };
        Ok(RespBuf::Response(response))
    }

    fn read_input(&mut self) -> Result<RawInput> {
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
            Err(err) => bail!("spi: {}", convert_error(err)),
        }
    }

    fn get_partition(&mut self) -> Result<u8> {
        let mut buf = [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
        let parts = read_partition_table(&mut self.flash, &mut buf)?;
        let part_type = PartitionType::Data(DataPartitionSubType::Ota);
        let ota_part = parts.find_partition(part_type)?;
        let Some(ota_part) = ota_part else {
            bail!("cannot find OTA data partition");
        };
        let mut ota_part = ota_part.as_embedded_storage(&mut self.flash);
        let mut ota = Ota::new(&mut ota_part, 2)?;
        let part = ota.current_app_partition()?;
        let part = match part {
            AppPartitionSubType::Factory => 0,
            AppPartitionSubType::Ota0 => 1,
            AppPartitionSubType::Ota1 => 2,
            _ => unreachable!(),
        };
        Ok(part)
    }

    fn switch_partition(&mut self, part: u8) -> Result<()> {
        let mut buf = [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
        let parts = read_partition_table(&mut self.flash, &mut buf)?;
        let part_type = PartitionType::Data(DataPartitionSubType::Ota);
        let ota_part = parts.find_partition(part_type)?;
        let Some(ota_part) = ota_part else {
            bail!("cannot find OTA data partition");
        };
        let mut ota_part = ota_part.as_embedded_storage(&mut self.flash);
        let mut ota = Ota::new(&mut ota_part, 2)?;

        let part = match part {
            0 | 10 => AppPartitionSubType::Factory,
            1 | 11 => AppPartitionSubType::Ota0,
            2 | 12 => AppPartitionSubType::Ota1,
            _ => bail!("selected partition is out of range"),
        };
        ota.set_current_app_partition(part)?;
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        self.wifi.start()?;
        self.manager.set_channel(6)?;
        // self.manager.set_rate(WifiPhyRate::Rate54m)?;
        retries::start()?;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.wifi.stop()?;
        while let Ok(peer) = self.manager.fetch_peer(true) {
            self.manager.remove_peer(&peer.peer_address)?;
        }
        retries::stop()?;
        Ok(())
    }

    fn local_addr() -> Addr {
        esp_radio::wifi::sta_mac()
    }

    fn advertise() {
        Self::send(BROADCAST_ADDRESS, b"HELLO");
    }

    fn recv(&self) -> Result<Option<(Addr, Box<[u8]>)>> {
        let Some(packet) = self.receiver.receive() else {
            return Ok(None);
        };

        let known_peer = self.manager.peer_exists(&packet.info.src_address);
        if !known_peer {
            if packet.data() == b"HELLO" {
                let peer = PeerInfo {
                    peer_address: packet.info.src_address,
                    lmk: None,
                    channel: None,
                    encrypt: false,
                    interface: EspNowWifiInterface::Sta,
                };
                self.manager.add_peer(peer)?;
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

const fn convert_error(
    value: embedded_hal_bus::spi::DeviceError<esp_hal::spi::Error, Infallible>,
) -> &'static str {
    use esp_hal::dma::DmaError;
    let embedded_hal_bus::spi::DeviceError::Spi(err) = value;
    match err {
        esp_hal::spi::Error::DmaError(err) => match err {
            DmaError::InvalidAlignment(_) => "dmi: invalid alignment",
            DmaError::OutOfDescriptors => "dmi: out of descriptors",
            DmaError::DescriptorError => "dmi: descriptor error",
            DmaError::Overflow => "dmi: overflow",
            DmaError::BufferTooSmall => "dmi: buffer too small",
            DmaError::UnsupportedMemoryRegion => "dmi: unsupported_memory_region",
            DmaError::InvalidChunkSize => "dmi: invalid chunk size",
            DmaError::Late => "dmi: late",
        },
        esp_hal::spi::Error::MaxDmaTransferSizeExceeded => {
            "the maximum DMA transfer size was exceeded"
        }
        esp_hal::spi::Error::FifoSizeExeeded => {
            "the FIFO size was exceeded during SPI communication"
        }
        esp_hal::spi::Error::Unsupported => "the operation is unsupported",
        esp_hal::spi::Error::Unknown => "unknown error occurred during SPI communication",
        _ => "unknown error",
    }
}

fn get_firmware_version() -> (u8, u8, u8) {
    let raw = env!("CARGO_PKG_VERSION");
    let mut iter = raw.split('.');
    let major: u8 = iter.next().unwrap().parse().unwrap();
    let minor: u8 = iter.next().unwrap().parse().unwrap();
    let patch: u8 = iter.next().unwrap().parse().unwrap();
    (major, minor, patch)
}
