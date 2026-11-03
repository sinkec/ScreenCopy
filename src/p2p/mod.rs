//! Wi-Fi Direct discovery and connection through NetworkManager's D-Bus API.
//!
//! NetworkManager is used rather than wpa_supplicant directly because it needs
//! no extra privileges for a local user and also assigns the IP addresses. The
//! connection is bound to this program's D-Bus connection, so NetworkManager
//! ends it by itself if the program goes away for any reason.

pub mod wfd_ie;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gtk::glib;
use thiserror::Error;
use tracing::{debug, info, warn};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::firewall::protocol::is_valid_p2p_interface;
use wfd_ie::{DeviceInfo, parse_device_info};

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_MANAGER: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const NM_DEVICE_P2P: &str = "org.freedesktop.NetworkManager.Device.WifiP2P";
const NM_PEER: &str = "org.freedesktop.NetworkManager.WifiP2PPeer";
const NM_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const NM_IP4: &str = "org.freedesktop.NetworkManager.IP4Config";

const DEVICE_TYPE_WIFI_P2P: u32 = 30;
const ACTIVE_STATE_ACTIVATED: u32 = 2;
const ACTIVE_STATE_DEACTIVATING: u32 = 3;
const ACTIVE_STATE_DEACTIVATED: u32 = 4;

/// NetworkManager stops searching by itself after this many seconds.
pub const SEARCH_SECONDS: i32 = 30;
/// Long enough for someone to walk to the TV and accept the request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Names come over the air, so they are limited before reaching the screen.
const MAX_NAME_CHARACTERS: usize = 64;

#[derive(Debug, Error)]
pub enum P2pError {
    #[error("cannot talk to NetworkManager: {0}")]
    Bus(#[from] zbus::Error),
    #[error("NetworkManager refused the request: {0}")]
    Refused(#[from] zbus::fdo::Error),
    #[error("this computer has no Wi-Fi Direct device")]
    NoDevice,
    #[error("the device reported an invalid hardware address")]
    InvalidPeer,
    #[error("the TV did not accept the connection in time")]
    Timeout,
    #[error("the connection to the TV failed")]
    ConnectionFailed,
    #[error("the connection was cancelled")]
    Cancelled,
    #[error("the connection has no usable {0}")]
    MissingNetworkData(&'static str),
}

/// A nearby device that can receive Miracast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    path: OwnedObjectPath,
    pub name: String,
    pub hw_address: String,
    pub strength: u8,
    pub info: DeviceInfo,
}

#[cfg(test)]
impl Peer {
    pub(crate) fn for_test(hw_address: &str, name: &str, info: DeviceInfo) -> Self {
        Self {
            path: OwnedObjectPath::default(),
            name: name.to_owned(),
            hw_address: hw_address.to_owned(),
            strength: 0,
            info,
        }
    }
}

/// An established Wi-Fi Direct connection.
#[derive(Debug)]
pub struct P2pLink {
    active_connection: OwnedObjectPath,
    pub interface: String,
    pub local_address: Ipv4Addr,
}

/// Keep this alive for as long as a link is in use: dropping it closes the
/// D-Bus connection, and with that NetworkManager ends the link.
pub struct P2pClient {
    connection: zbus::Connection,
    device: OwnedObjectPath,
}

impl P2pClient {
    pub async fn new() -> Result<Self, P2pError> {
        let connection = zbus::Connection::system().await?;
        let manager = proxy(&connection, NM_PATH, NM_MANAGER).await?;
        let devices: Vec<OwnedObjectPath> = manager.call("GetAllDevices", &()).await?;
        for device in devices {
            let properties = proxy(&connection, device.as_str(), NM_DEVICE).await?;
            if properties.get_property::<u32>("DeviceType").await? == DEVICE_TYPE_WIFI_P2P {
                debug!(device = device.as_str(), "found Wi-Fi Direct device");
                return Ok(Self { connection, device });
            }
        }
        Err(P2pError::NoDevice)
    }

    async fn device_p2p(&self) -> Result<zbus::Proxy<'_>, P2pError> {
        proxy(&self.connection, self.device.as_str(), NM_DEVICE_P2P).await
    }

    pub async fn start_search(&self) -> Result<(), P2pError> {
        let options = HashMap::from([("timeout", Value::from(SEARCH_SECONDS))]);
        let _: () = self
            .device_p2p()
            .await?
            .call("StartFind", &(options))
            .await?;
        debug!("started searching");
        Ok(())
    }

    pub async fn stop_search(&self) -> Result<(), P2pError> {
        let _: () = self.device_p2p().await?.call("StopFind", &()).await?;
        debug!("stopped searching");
        Ok(())
    }

    /// The Miracast receivers found so far, sorted by name.
    pub async fn peers(&self) -> Result<Vec<Peer>, P2pError> {
        let paths: Vec<OwnedObjectPath> = self.device_p2p().await?.get_property("Peers").await?;
        let mut peers = Vec::with_capacity(paths.len());
        for path in paths {
            // A peer can vanish at any moment, which is not an error.
            match self.read_peer(path).await {
                Ok(Some(peer)) => peers.push(peer),
                Ok(None) => {}
                Err(error) => debug!(%error, "cannot read peer"),
            }
        }
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(peers)
    }

    async fn read_peer(&self, path: OwnedObjectPath) -> Result<Option<Peer>, P2pError> {
        let peer = proxy(&self.connection, path.as_str(), NM_PEER).await?;
        let name: String = peer.get_property("Name").await?;
        let hw_address: String = peer.get_property("HwAddress").await?;
        let strength: u8 = peer.get_property("Strength").await?;
        let ies: Vec<u8> = peer.get_property("WfdIEs").await?;
        debug!(path = path.as_str(), %name, %hw_address, strength, ?ies, "peer");

        let Some(info) = parse_device_info(&ies).filter(DeviceInfo::is_sink) else {
            return Ok(None);
        };
        if !is_valid_hw_address(&hw_address) {
            warn!("ignoring a peer with an invalid hardware address");
            return Ok(None);
        }
        Ok(Some(Peer {
            path,
            name: display_name(&name, &hw_address),
            hw_address,
            strength,
            info,
        }))
    }

    /// Connects to `peer` and waits for the network to be ready. The TV usually
    /// asks for confirmation first, so this can take up to a minute. Setting
    /// `cancel` ends the attempt.
    pub async fn connect(&self, peer: &Peer, cancel: &AtomicBool) -> Result<P2pLink, P2pError> {
        let settings = connection_settings(&peer.hw_address, glib::user_name().to_str());
        let options = HashMap::from([
            ("persist", Value::from("volatile")),
            // NetworkManager ends the connection when this program disappears.
            ("bind-activation", Value::from("dbus-client")),
        ]);
        let manager = proxy(&self.connection, NM_PATH, NM_MANAGER).await?;
        let (_, active, _): (
            OwnedObjectPath,
            OwnedObjectPath,
            HashMap<String, OwnedValue>,
        ) = manager
            .call(
                "AddAndActivateConnection2",
                &(
                    settings,
                    ObjectPath::from(&self.device),
                    ObjectPath::from(&peer.path),
                    options,
                ),
            )
            .await?;
        info!(peer = %peer.hw_address, "connecting");

        match self.wait_until_activated(&active, cancel).await {
            Ok(()) => self.read_link(active).await,
            Err(error) => {
                self.deactivate(&active).await;
                Err(error)
            }
        }
    }

    async fn wait_until_activated(
        &self,
        active: &OwnedObjectPath,
        cancel: &AtomicBool,
    ) -> Result<(), P2pError> {
        let started = Instant::now();
        let connection = proxy(&self.connection, active.as_str(), NM_ACTIVE).await?;
        let mut last_state = None;
        while started.elapsed() < CONNECT_TIMEOUT {
            if cancel.load(Ordering::Relaxed) {
                info!("connecting was cancelled");
                return Err(P2pError::Cancelled);
            }
            // The object disappears when a failed connection is cleaned up.
            let state: u32 = connection
                .get_property("State")
                .await
                .map_err(|_| P2pError::ConnectionFailed)?;
            if last_state != Some(state) {
                debug!(state, "connection state");
                last_state = Some(state);
            }
            match state {
                ACTIVE_STATE_ACTIVATED => return Ok(()),
                ACTIVE_STATE_DEACTIVATING | ACTIVE_STATE_DEACTIVATED => {
                    self.log_failure_reason().await;
                    return Err(P2pError::ConnectionFailed);
                }
                _ => glib::timeout_future(CONNECT_POLL_INTERVAL).await,
            }
        }
        Err(P2pError::Timeout)
    }

    async fn log_failure_reason(&self) {
        if let Ok(device) = proxy(&self.connection, self.device.as_str(), NM_DEVICE).await
            && let Ok(reason) = device.get_property::<(u32, u32)>("StateReason").await
        {
            warn!(state = reason.0, reason = reason.1, "connection failed");
        }
    }

    async fn read_link(&self, active: OwnedObjectPath) -> Result<P2pLink, P2pError> {
        let device = proxy(&self.connection, self.device.as_str(), NM_DEVICE).await?;
        let interface: String = device.get_property("IpInterface").await?;
        if !is_valid_p2p_interface(&interface) {
            warn!(%interface, "unexpected interface name");
            self.deactivate(&active).await;
            return Err(P2pError::MissingNetworkData("interface"));
        }

        let connection = proxy(&self.connection, active.as_str(), NM_ACTIVE).await?;
        let config: OwnedObjectPath = connection.get_property("Ip4Config").await?;
        let config = proxy(&self.connection, config.as_str(), NM_IP4).await?;
        let addresses: Vec<HashMap<String, OwnedValue>> =
            config.get_property("AddressData").await?;
        let Some(local_address) = first_ipv4_address(&addresses) else {
            self.deactivate(&active).await;
            return Err(P2pError::MissingNetworkData("IPv4 address"));
        };
        info!(%interface, %local_address, "connected");
        Ok(P2pLink {
            active_connection: active,
            interface,
            local_address,
        })
    }

    pub async fn disconnect(&self, link: P2pLink) {
        self.deactivate(&link.active_connection).await;
        info!("disconnected");
    }

    async fn deactivate(&self, active: &OwnedObjectPath) {
        let result = async {
            let manager = proxy(&self.connection, NM_PATH, NM_MANAGER).await?;
            let _: () = manager
                .call("DeactivateConnection", &(ObjectPath::from(active)))
                .await?;
            Ok::<(), P2pError>(())
        };
        // It may already be gone, which is the result we wanted.
        if let Err(error) = result.await {
            debug!(%error, "cannot deactivate the connection");
        }
    }
}

async fn proxy<'a>(
    connection: &'a zbus::Connection,
    path: &'a str,
    interface: &'static str,
) -> Result<zbus::Proxy<'a>, P2pError> {
    // Without a cache, properties are always current and no signals are needed.
    Ok(zbus::proxy::Builder::new(connection)
        .destination(NM_SERVICE)?
        .path(path)?
        .interface(interface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?)
}

fn connection_settings<'a>(
    hw_address: &'a str,
    user: Option<&str>,
) -> HashMap<&'static str, HashMap<&'static str, Value<'a>>> {
    let mut general = HashMap::from([
        ("id", Value::from("MonitorBeam")),
        ("type", Value::from("wifi-p2p")),
        ("autoconnect", Value::from(false)),
    ]);
    // Limits the connection to this user, so it needs no administrator rights.
    if let Some(user) = user {
        general.insert("permissions", Value::from(vec![format!("user:{user}:")]));
    }
    HashMap::from([
        ("connection", general),
        (
            "wifi-p2p",
            HashMap::from([
                ("peer", Value::from(hw_address)),
                ("wfd-ies", Value::from(wfd_ie::SOURCE_IES.to_vec())),
            ]),
        ),
        (
            "ipv4",
            HashMap::from([
                ("method", Value::from("auto")),
                // The TV must never become the route to the internet.
                ("never-default", Value::from(true)),
            ]),
        ),
        (
            "ipv6",
            HashMap::from([
                ("method", Value::from("auto")),
                ("may-fail", Value::from(true)),
            ]),
        ),
    ])
}

fn first_ipv4_address(addresses: &[HashMap<String, OwnedValue>]) -> Option<Ipv4Addr> {
    addresses.iter().find_map(|entry| {
        let address = entry.get("address")?.downcast_ref::<&str>().ok()?;
        address.parse().ok()
    })
}

pub(crate) fn is_valid_hw_address(address: &str) -> bool {
    let groups: Vec<&str> = address.split(':').collect();
    groups.len() == 6
        && groups
            .iter()
            .all(|group| group.len() == 2 && group.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Removes control characters and limits the length of a name from the air.
fn display_name(name: &str, hw_address: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_NAME_CHARACTERS)
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        hw_address.to_owned()
    } else {
        clean.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: &str = "56:3A:D6:DE:A9:62";

    #[test]
    fn accepts_hardware_addresses() {
        assert!(is_valid_hw_address(ADDRESS));
        assert!(is_valid_hw_address("00:11:22:aa:bb:cc"));
    }

    #[test]
    fn rejects_malformed_hardware_addresses() {
        for address in [
            "",
            "56:3A:D6:DE:A9",
            "56:3A:D6:DE:A9:62:00",
            "563AD6DEA962",
            "56:3A:D6:DE:A9:6",
            "56:3A:D6:DE:A9:GG",
            "56:3A:D6:DE:A9:62\n",
        ] {
            assert!(!is_valid_hw_address(address), "{address:?}");
        }
    }

    #[test]
    fn keeps_a_normal_name() {
        assert_eq!(
            display_name("Samsung AU7172 75 TV", ADDRESS),
            "Samsung AU7172 75 TV"
        );
    }

    #[test]
    fn cleans_hostile_names() {
        assert_eq!(display_name("TV\n\u{1b}[31m\0", ADDRESS), "TV[31m");
        assert_eq!(display_name(&"x".repeat(1000), ADDRESS).len(), 64);
        assert_eq!(display_name("<b>TV</b>", ADDRESS), "<b>TV</b>");
    }

    #[test]
    fn empty_name_falls_back_to_the_address() {
        assert_eq!(display_name("", ADDRESS), ADDRESS);
        assert_eq!(display_name(" \n\t ", ADDRESS), ADDRESS);
    }

    #[test]
    fn settings_describe_a_volatile_safe_connection() {
        let settings = connection_settings(ADDRESS, Some("someone"));
        assert_eq!(settings["wifi-p2p"]["peer"], Value::from(ADDRESS));
        assert_eq!(
            settings["wifi-p2p"]["wfd-ies"],
            Value::from(wfd_ie::SOURCE_IES.to_vec())
        );
        assert_eq!(settings["connection"]["type"], Value::from("wifi-p2p"));
        assert_eq!(settings["connection"]["autoconnect"], Value::from(false));
        assert_eq!(settings["ipv4"]["never-default"], Value::from(true));
        assert_eq!(
            settings["connection"]["permissions"],
            Value::from(vec!["user:someone:".to_owned()])
        );
    }

    #[test]
    fn settings_work_without_a_user_name() {
        let settings = connection_settings(ADDRESS, None);
        assert!(!settings["connection"].contains_key("permissions"));
    }

    #[test]
    fn reads_the_first_valid_address() {
        let entry = |address: &str| {
            HashMap::from([(
                "address".to_owned(),
                OwnedValue::try_from(Value::from(address)).unwrap(),
            )])
        };
        let addresses = [entry("nonsense"), entry("10.42.0.1"), entry("10.42.0.2")];
        assert_eq!(
            first_ipv4_address(&addresses),
            Some(Ipv4Addr::new(10, 42, 0, 1))
        );
        assert_eq!(first_ipv4_address(&[]), None);
    }
}
