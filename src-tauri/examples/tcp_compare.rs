//! Compare the application's TCP probe with the standard library's blocking
//! connect on one explicitly supplied destination. Does not alter app history.

use ip_picker_lib::stability::{probe_tcp, summarize};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let Some(address) = arguments
        .first()
        .and_then(|value| value.parse::<SocketAddr>().ok())
    else {
        eprintln!("Usage: tcp_compare <IPv4:port> [count=20]");
        std::process::exit(2);
    };
    let count = match arguments.get(1) {
        Some(value) => value.parse::<usize>().ok(),
        None => Some(20),
    }
    .filter(|value| (1..=100).contains(value));
    let Some(count) = count else {
        std::process::exit(2);
    };
    let cadence = Duration::from_millis(200);
    let timeout = Duration::from_millis(1500);
    let started_at = chrono::Utc::now().to_rfc3339();
    let route_before = match address.ip() {
        std::net::IpAddr::V4(ip) => ip_picker_lib::network::capture(ip),
        _ => None,
    };
    let baseline = tokio::task::spawn_blocking(move || {
        let mut samples = Vec::with_capacity(count);
        for index in 0..count {
            let started = Instant::now();
            let sample = std::net::TcpStream::connect_timeout(&address, timeout)
                .ok()
                .map(|stream| {
                    let milliseconds = started.elapsed().as_secs_f64() * 1000.0;
                    drop(stream);
                    milliseconds
                });
            samples.push(sample);
            if index + 1 < count {
                std::thread::sleep(cadence.saturating_sub(started.elapsed()));
            }
        }
        summarize(count, 200.0, samples)
    });
    let application = probe_tcp(
        address,
        count,
        cadence,
        timeout,
        200.0,
        CancellationToken::new(),
        |_| {},
    );
    let (application, baseline) = tokio::join!(application, baseline);
    let output = serde_json::json!({
        "address": address.to_string(),
        "startedUnderCurrentNetworkOnly": true,
        "startedAt": started_at,
        "finishedAt": chrono::Utc::now().to_rfc3339(),
        "routeBefore": route_before,
        "routeAfter": match address.ip() {
            std::net::IpAddr::V4(ip) => ip_picker_lib::network::capture(ip),
            _ => None,
        },
        "applicationProbe": application,
        "blockingSocketBaseline": baseline.unwrap(),
    });
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}
