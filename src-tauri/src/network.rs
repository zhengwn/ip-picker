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
    pub captured_at: String,
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
    from_route_output(ip, text, captured_at)
}

#[cfg(not(target_os = "macos"))]
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
        assert_eq!(path.captured_at, "2026-10-23T04:00:00Z");
        let physical = from_route_output(ip, "  interface: en0\n", "now".into()).unwrap();
        assert!(!physical.tunnel_interface);
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
}
