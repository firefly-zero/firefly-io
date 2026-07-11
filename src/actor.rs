use crate::{retries, send_resp_buf, wifi::WifiManager};
use alloc::{boxed::Box, string::String};
use cirque_pinnacle::{Absolute, Touchpad};
use core::convert::Infallible;
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_io::Read;
use embedded_storage::Storage;
use esp_bootloader_esp_idf::{
    ota::Ota,
    partitions::{read_partition_table, AppPartitionSubType, DataPartitionSubType, PartitionType},
};
use esp_hal::{
    delay::Delay,
    gpio::{Input, Output},
    spi::master::Spi,
    uart::Uart,
    Blocking,
};
use esp_println::println;
use esp_radio::esp_now::*;
use esp_storage::FlashStorage;
use firefly_types::{spi::*, Encode};

type PadSpi<'a> = ExclusiveDevice<Spi<'a, Blocking>, Output<'a>, Delay>;
type RawInput = (Option<(u16, u16)>, u8);

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

    pub fn handle(&mut self, req: Request, uart: &mut Uart<'_, Blocking>) -> RespBuf<'_> {
        match self.handle_inner(req, uart) {
            Ok(resp) => resp,
            Err(err) => {
                println!("error: {err:?}");
                RespBuf::Response(Response::Error(err))
            }
        }
    }

    fn handle_inner<'b>(
        &mut self,
        req: Request,
        uart: &mut Uart<'_, Blocking>,
    ) -> Result<RespBuf<'b>, &'static str> {
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
                let partition = 0;
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
            Request::PartitionWrite(part, len) => {
                self.write_partition(part, len, uart)?;
                Response::PartitionWritten
            }
            Request::PartitionChunk(_) => {
                return Err("unexpected partition chunk");
            }
            Request::PartitionSwitch(part) => {
                self.switch_partition(part)?;
                Response::PartitionSwitched
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

    #[expect(clippy::cast_possible_truncation)]
    pub fn write_partition(
        &mut self,
        part: u8,
        len: u32,
        uart: &mut Uart<'_, Blocking>,
    ) -> Result<(), &'static str> {
        let mut buf = [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
        let Ok(parts) = read_partition_table(&mut self.flash, &mut buf) else {
            return Err("failed to read partition table");
        };
        let part = match part {
            0 | 10 => AppPartitionSubType::Factory,
            1 | 11 => AppPartitionSubType::Ota0,
            2 | 12 => AppPartitionSubType::Ota1,
            _ => return Err("selected partition is out of range"),
        };
        let Ok(partition) = parts.find_partition(PartitionType::App(part)) else {
            return Err("cannot read partitions");
        };
        let Some(partition) = partition else {
            return Err("cannot find runtime partition");
        };
        let mut storage = partition.as_embedded_storage(&mut self.flash);

        let mut buf = [0u8; 4096];
        let mut written = 0;
        while written != len {
            // read request size
            _ = uart.read(&mut buf[..1]);
            let size = usize::from(buf[0]);

            // read request payload
            // TODO(@orsinium): don't unwrap
            uart.read_exact(&mut buf[..size]).unwrap();
            let req = Request::decode(&buf[..size]).unwrap();

            let resp = if let Request::PartitionChunk(chunk) = req {
                let res = storage.write(written, chunk);
                if res.is_err() {
                    return Err("failed to write firmware into partition");
                }
                written += chunk.len() as u32;
                Response::PartitionChunk
            } else {
                Response::Error("unexpected request, expected partition chunk")
            };
            let resp = RespBuf::Response(resp);
            _ = send_resp_buf(uart, &mut buf, resp);
        }
        Ok(())
    }

    fn switch_partition(&mut self, part: u8) -> Result<(), &'static str> {
        let mut buf = [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
        let Ok(parts) = read_partition_table(&mut self.flash, &mut buf) else {
            return Err("failed to read partition table");
        };
        let part_type = PartitionType::Data(DataPartitionSubType::Ota);
        let Ok(ota_part) = parts.find_partition(part_type) else {
            return Err("cannot read partitions");
        };
        let Some(ota_part) = ota_part else {
            return Err("cannot find OTA data partition");
        };
        let mut ota_part = ota_part.as_embedded_storage(&mut self.flash);
        let Ok(mut ota) = Ota::new(&mut ota_part, 2) else {
            return Err("OTA partition is invalid");
        };

        let part = match part {
            0 | 10 => AppPartitionSubType::Factory,
            1 | 11 => AppPartitionSubType::Ota0,
            2 | 12 => AppPartitionSubType::Ota1,
            _ => return Err("selected partition is out of range"),
        };
        let res = ota.set_current_app_partition(part);
        if res.is_err() {
            return Err("failed to set OTA partition");
        }
        Ok(())
    }
}

pub type Addr = [u8; 6];
type NetworkResult<T> = Result<T, &'static str>;

// Input- and Multiplayer-related methods.
impl Actor<'_> {
    fn start(&mut self) -> NetworkResult<()> {
        self.wifi.start()?;
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

    fn stop(&mut self) -> NetworkResult<()> {
        self.wifi.stop()?;
        while let Ok(peer) = self.manager.fetch_peer(true) {
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

fn get_firmware_version() -> (u8, u8, u8) {
    let raw = env!("CARGO_PKG_VERSION");
    let mut iter = raw.split('.');
    let major: u8 = iter.next().unwrap().parse().unwrap();
    let minor: u8 = iter.next().unwrap().parse().unwrap();
    let patch: u8 = iter.next().unwrap().parse().unwrap();
    (major, minor, patch)
}
