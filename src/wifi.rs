use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use esp_radio::wifi::{PowerSaveMode, ScanConfig, WifiController, WifiDevice, WifiError};
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::{dhcpv4, tcp},
    wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint},
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
impl<'a> WifiManager<'a> {
    pub fn new(device: WifiDevice<'a>, controller: WifiController<'a>) -> Self {
        let mut device = device;
        let addr = device.mac_address();
        let addr = EthernetAddress(addr);
        let config = smoltcp::iface::Config::new(addr.into());
        let now = smoltcp::time::Instant::from_micros(1);
        let iface = smoltcp::iface::Interface::new(config, &mut device, now);
        let rbuf = tcp::SocketBuffer::new(vec![0; 1024]);
        let tbuf = tcp::SocketBuffer::new(vec![0; 1024]);
        let tcp_socket = tcp::Socket::new(rbuf, tbuf);
        let dhcp_socket = dhcpv4::Socket::new();
        let mut sockets = SocketSet::new(alloc::vec::Vec::new());
        let tcp_ref = sockets.add(tcp_socket);
        let dhcp_ref = sockets.add(dhcp_socket);
        Self {
            controller,
            sockets,
            tcp_ref,
            dhcp_ref,
            iface,
            device,
        }
    }

    /// Ensure the wifi controller is started.
    ///
    /// Must be called before connecting to an AP or starting esp-now.
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

    /// Stop the wifi controller to save energy.
    pub fn stop(&mut self) -> NetworkResult<()> {
        let res = self.controller.stop();
        if res.is_err() {
            return Err("failed to stop wifi");
        }
        Ok(())
    }

    fn poll(&mut self) {
        let now = esp_hal::time::Instant::now();
        let now = now.duration_since_epoch().as_micros();
        #[expect(clippy::cast_possible_wrap)]
        let now = smoltcp::time::Instant::from_micros(now as i64);
        self.iface.poll(now, &mut self.device, &mut self.sockets);
    }

    /// Scan for available wifi Access Points.
    ///
    /// Performs an active scan: switches to every channel in order,
    /// sends a beacon, and waits for a response for 10-20ms.
    ///
    /// Returns the first 6 APs that it can find. Usually these are
    /// the points with the strongest signal but not necessarily.
    /// Scan again and the list might be slightly different.
    /// The limitation comes from the max packet size in our SPI implementation.
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

    /// Connect to the given wifi Access Point.
    ///
    /// Non-blocking. Check the status to see if connected.
    ///
    /// * Auth method: WPA-2 PSK.
    /// * Protocol: 802.11b, 802.11b/g, 802.11b/g/n.
    /// * Channel: auto-detected
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

    /// Get the wifi/DHCP connection status.
    ///
    /// Since "connect" is non-blocking and esp-radio doesn't provide
    /// status for failed connection (only "disconnected"), make sure
    /// to ignore "disconnected" status for a while after calling "connect".
    pub fn status(&mut self) -> NetworkResult<u8> {
        match self.controller.is_connected() {
            Ok(false) => Ok(1),
            Err(WifiError::Disconnected) => Ok(2),
            Ok(true) => {
                self.poll();
                self.dhcp_poll();
                if self.iface.ip_addrs().is_empty() {
                    Ok(3)
                } else {
                    Ok(4)
                }
            }
            Err(_) => Err("failed to connect to wifi"),
        }
    }

    /// Disconnect from the wifi Access Point.
    pub fn disconnect(&mut self) -> NetworkResult<()> {
        let res = self.controller.disconnect();
        if res.is_err() {
            return Err("failed to disconnect from wifi");
        }
        Ok(())
    }

    fn dhcp_poll(&mut self) {
        use dhcpv4::Event;

        let socket: &mut dhcpv4::Socket = self.sockets.get_mut(self.dhcp_ref);
        let event = socket.poll();
        let Some(event) = event else {
            return;
        };
        match event {
            Event::Configured(config) => {
                self.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let addr = IpCidr::Ipv4(config.address);
                    addrs.push(addr).unwrap();
                });
                if let Some(router) = config.router {
                    let routes = self.iface.routes_mut();
                    routes.add_default_ipv4_route(router).unwrap();
                }
            }
            Event::Deconfigured => {
                #[expect(clippy::redundant_closure_for_method_calls)]
                self.iface.update_ip_addrs(|addrs| addrs.clear());
                let routes = self.iface.routes_mut();
                routes.remove_default_ipv4_route();
            }
        }
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
        self.poll();
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

    pub fn tcp_recv(&mut self) -> NetworkResult<Box<[u8]>> {
        let socket: &mut tcp::Socket = self.sockets.get_mut(self.tcp_ref);
        if !socket.may_recv() {
            return Err("trying to read from dead TCP connection");
        }
        let mut buf = vec![0; 160];
        let Ok(n) = socket.recv_slice(&mut buf) else {
            return Err("failed to read incoming TCP data");
        };
        buf.truncate(n);
        Ok(buf.into_boxed_slice())
    }

    pub fn tcp_close(&mut self) {
        let socket: &mut tcp::Socket = self.sockets.get_mut(self.tcp_ref);
        socket.abort();
    }
}
