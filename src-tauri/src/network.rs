//! A read-only, per-run route sample. This describes the sampled destination at
//! capture time; it cannot prove that every destination bypasses a VPN or proxy.

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPath {
    pub sample_ip: String,
    pub interface_name: String,
    pub tunnel_interface: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_kind: Option<InterfaceKind>,
    pub captured_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InterfaceKind {
    Wired,
    Wifi,
    Tunnel,
    Other,
}

#[cfg(target_os = "macos")]
pub fn capture(ip: Ipv4Addr) -> Option<NetworkPath> {
    // Read the existing route. Never alter routes, bind to a different network,
    // disable a VPN, or send a request to a third-party diagnostic service.
    let captured_at = chrono::Utc::now().to_rfc3339();
    let output = std::process::Command::new("/sbin/route")
        .args(["-n", "get", &ip.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    let mut path = from_route_output(ip, text, captured_at)?;
    if !path.tunnel_interface {
        path.interface_kind = macos_interface_kind(&path.interface_name);
    }
    Some(path)
}

#[cfg(target_os = "macos")]
fn macos_interface_kind(interface_name: &str) -> Option<InterfaceKind> {
    use system_configuration::network_configuration::{get_interfaces, SCNetworkInterfaceType};

    let interfaces = get_interfaces();
    let interface = interfaces.iter().find(|interface| {
        interface
            .bsd_name()
            .is_some_and(|name| name == interface_name)
    })?;
    match interface.interface_type()? {
        SCNetworkInterfaceType::Ethernet => Some(InterfaceKind::Wired),
        SCNetworkInterfaceType::IEEE80211 => Some(InterfaceKind::Wifi),
        _ => Some(InterfaceKind::Other),
    }
}

#[cfg(windows)]
pub fn capture(ip: Ipv4Addr) -> Option<NetworkPath> {
    use windows_sys::Win32::{
        Foundation::NO_ERROR,
        NetworkManagement::IpHelper::{GetBestInterface, GetIfEntry2, MIB_IF_ROW2},
    };

    let captured_at = chrono::Utc::now().to_rfc3339();
    // IPAddr stores the network-order octets in memory.
    let address = u32::from_ne_bytes(ip.octets());
    let mut index = 0;
    // SAFETY: index is writable for the duration of the synchronous query.
    if unsafe { GetBestInterface(address, &mut index) } != NO_ERROR {
        return None;
    }

    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    // SAFETY: row contains a valid interface index and is writable for the query.
    let (interface_name, interface_kind) = if unsafe { GetIfEntry2(&mut row) } == NO_ERROR {
        let alias = row
            .Alias
            .split(|&unit| unit == 0)
            .next()
            .unwrap_or_default();
        let alias = String::from_utf16_lossy(alias).trim().to_owned();
        let interface_name = if alias.is_empty() {
            format!("if{index}")
        } else {
            alias
        };
        (interface_name, Some(windows_interface_kind(&row)))
    } else {
        (format!("if{index}"), None)
    };
    Some(NetworkPath {
        sample_ip: ip.to_string(),
        interface_name,
        tunnel_interface: interface_kind == Some(InterfaceKind::Tunnel),
        interface_kind,
        captured_at,
    })
}

#[cfg(windows)]
fn windows_interface_kind(
    row: &windows_sys::Win32::NetworkManagement::IpHelper::MIB_IF_ROW2,
) -> InterfaceKind {
    use windows_sys::Win32::NetworkManagement::{
        IpHelper::{IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IF_TYPE_TUNNEL},
        Ndis::{NdisPhysicalMediumNative802_11, NdisPhysicalMediumWirelessLan, TUNNEL_TYPE_NONE},
    };

    // The first bit of InterfaceAndOperStatusFlags denotes a hardware interface.
    // Virtual adapters may also report IF_TYPE_ETHERNET_CSMACD.
    let hardware = row.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
    if row.Type == IF_TYPE_TUNNEL || row.TunnelType != TUNNEL_TYPE_NONE {
        InterfaceKind::Tunnel
    } else if row.Type == IF_TYPE_IEEE80211
        || row.PhysicalMediumType == NdisPhysicalMediumNative802_11
        || row.PhysicalMediumType == NdisPhysicalMediumWirelessLan
    {
        InterfaceKind::Wifi
    } else if row.Type == IF_TYPE_ETHERNET_CSMACD && hardware {
        InterfaceKind::Wired
    } else {
        InterfaceKind::Other
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn capture(_ip: Ipv4Addr) -> Option<NetworkPath> {
    // No observation is preferable to inferring a path we did not collect.
    None
}

#[cfg(any(target_os = "macos", test))]
fn from_route_output(ip: Ipv4Addr, output: &str, captured_at: String) -> Option<NetworkPath> {
    let interface = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))?
        .trim();
    if interface.is_empty()
        || !interface
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || b"_.-".contains(&value))
    {
        return None;
    }
    let tunnel_interface = ["utun", "tun", "tap", "ppp", "ipsec", "gif", "stf"]
        .iter()
        .any(|prefix| interface.starts_with(prefix));
    Some(NetworkPath {
        sample_ip: ip.to_string(),
        interface_name: interface.to_string(),
        tunnel_interface,
        interface_kind: tunnel_interface.then_some(InterfaceKind::Tunnel),
        captured_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_the_observed_interface_and_timestamp_without_claiming_all_routes() {
        let ip = "104.17.158.50".parse().unwrap();
        let path = from_route_output(
            ip,
            "route to: 104.17.158.50\n  interface: utun7\n flags: <UP,DONE>",
            "2026-10-23T04:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(path.sample_ip, ip.to_string());
        assert_eq!(path.interface_name, "utun7");
        assert!(path.tunnel_interface);
        assert_eq!(path.interface_kind, Some(InterfaceKind::Tunnel));
        assert_eq!(path.captured_at, "2026-10-23T04:00:00Z");
        let physical = from_route_output(ip, "  interface: en0\n", "now".into()).unwrap();
        assert!(!physical.tunnel_interface);
        assert_eq!(physical.interface_kind, None);
    }

    #[test]
    fn does_not_infer_a_route_from_missing_or_malformed_output() {
        for output in [
            "",
            "route: not found",
            "interface: ",
            "interface: en0 extra",
        ] {
            assert!(from_route_output(Ipv4Addr::LOCALHOST, output, "now".into()).is_none());
        }
    }

    #[test]
    fn older_route_snapshots_keep_the_recorded_name_without_guessing_the_type() {
        let path: NetworkPath = serde_json::from_value(serde_json::json!({
            "sampleIp": "104.17.158.50",
            "interfaceName": "en0",
            "tunnelInterface": false,
            "capturedAt": "2026-10-23T04:00:00Z"
        }))
        .unwrap();
        assert_eq!(path.interface_name, "en0");
        assert_eq!(path.interface_kind, None);
        assert!(serde_json::to_value(path)
            .unwrap()
            .get("interfaceKind")
            .is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_interface_type_uses_native_adapter_metadata() {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IF_TYPE_TUNNEL, MIB_IF_ROW2,
        };

        let mut row = MIB_IF_ROW2 {
            Type: IF_TYPE_ETHERNET_CSMACD,
            ..Default::default()
        };
        assert_eq!(windows_interface_kind(&row), InterfaceKind::Other);
        row.InterfaceAndOperStatusFlags._bitfield = 1;
        assert_eq!(windows_interface_kind(&row), InterfaceKind::Wired);
        row.Type = IF_TYPE_IEEE80211;
        assert_eq!(windows_interface_kind(&row), InterfaceKind::Wifi);
        row.Type = IF_TYPE_TUNNEL;
        assert_eq!(windows_interface_kind(&row), InterfaceKind::Tunnel);
    }
}
