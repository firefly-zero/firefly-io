use alloc::string::{String, ToString};
use esp_radio::wifi::{PowerSaveMode, ScanConfig, WifiController, WifiDevice, WifiError};
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::tcp,
    wire::{IpAddress, IpEndpoint},
};

pub struct WifiManager<'a> {
    pub controller: WifiController<'a>,
    pub sockets: SocketSet<'a>,
    pub tcp_ref: SocketHandle,
    pub dhcp_ref: SocketHandle,
    pub iface: smoltcp::iface::Interface,
    pub device: WifiDevice<'a>,
}

type NetworkResult<T> = Result<T, &'static str>;

// WiFi- and TCP-related methods.
impl WifiManager<'_> {
    pub fn start(&mut self) -> NetworkResult<()> {
        let res = self.controller.set_power_saving(PowerSaveMode::None);
        if res.is_err() {
            return Err("failed to exit power saving mode");
        }
        if !self.controller.is_started().unwrap_or_default() {
            let res = self.controller.start();
            if res.is_err() {
                return Err("failed to start wifi");
            }
        }
        Ok(())
    }

    pub fn stop(&mut self) -> NetworkResult<()> {
        let res = self.controller.stop();
        if res.is_err() {
            return Err("failed to stop wifi");
        }
        Ok(())
    }

    pub fn scan(&mut self) -> NetworkResult<[String; 6]> {
        self.start()?;
        let config = ScanConfig::default().with_max(6);
        let Ok(points) = self.controller.scan_with_config(config) else {
            return Err("failed to scan for networks");
        };
        let mut ssids = [const { String::new() }; 6];
        for (i, point) in points.into_iter().enumerate() {
            ssids[i] = point.ssid;
        }
        Ok(ssids)
    }

    pub fn connect(&mut self, ssid: &str, pass: &str) -> NetworkResult<()> {
        use esp_radio::wifi::*;
        let config = ClientConfig::default()
            .with_ssid(ssid.to_string())
            .with_password(pass.to_string());
        let config = ModeConfig::Client(config);
        let res = self.controller.set_config(&config);
        if res.is_err() {
            return Err("failed to set wifi config");
        }
        let res = self.controller.connect();
        if res.is_err() {
            return Err("failed to connect to wifi");
        }
        Ok(())
    }

    pub fn status(&self) -> NetworkResult<u8> {
        match self.controller.is_connected() {
            Ok(true) => Ok(1),
            Err(WifiError::Disconnected) => Ok(2),
            Ok(false) => Ok(3),
            Err(_) => Err("failed to connect to wifi"),
        }
    }

    pub fn disconnect(&mut self) -> NetworkResult<()> {
        let res = self.controller.disconnect();
        if res.is_err() {
            return Err("failed to disconnect from wifi");
        }
        Ok(())
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn tcp_connect(&mut self, ip: u32, port: u16) -> NetworkResult<()> {
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
        let socket: &mut tcp::Socket = self.sockets.get_mut(self.tcp_ref);
        let res = socket.connect(cx, remote_endpoint, local_endpoint);
        if res.is_err() {
            return Err("failed to connect to the TCP endpoint");
        }
        Ok(())
    }

    pub fn tcp_status(&mut self) -> u8 {
        self.tcp_poll();
        let socket: &mut tcp::Socket = self.sockets.get_mut(self.tcp_ref);
        match socket.state() {
            tcp::State::Closed => 1,
            tcp::State::Listen => 2,
            tcp::State::SynSent => 3,
            tcp::State::SynReceived => 4,
            tcp::State::Established => 5,
            tcp::State::FinWait1 => 6,
            tcp::State::FinWait2 => 7,
            tcp::State::CloseWait => 8,
            tcp::State::Closing => 9,
            tcp::State::LastAck => 10,
            tcp::State::TimeWait => 11,
        }
    }

    pub fn tcp_poll(&mut self) {
        let now = esp_hal::time::Instant::now();
        let now = now.duration_since_epoch().as_micros();
        #[expect(clippy::cast_possible_wrap)]
        let now = smoltcp::time::Instant::from_micros(now as i64);
        self.iface.poll(now, &mut self.device, &mut self.sockets);
    }

    pub fn tcp_send(&mut self, data: &[u8]) -> NetworkResult<u8> {
        let socket: &mut tcp::Socket = self.sockets.get_mut(self.tcp_ref);
        let Ok(n) = socket.send_slice(data) else {
            return Err("failed to send TCP data");
        };
        let socket: &mut tcp::Socket = self.sockets.get_mut(self.tcp_ref);
        socket.close();
        #[expect(clippy::cast_possible_truncation)]
        Ok(n as u8)
    }

    pub fn tcp_close(&mut self) {
        let socket: &mut tcp::Socket = self.sockets.get_mut(self.tcp_ref);
        socket.abort();
    }
}
