//! Minimal stand-in for a LowBand UI shell: connect to the lowbandd IPC
//! socket and print the governor events it broadcasts.
//!
//! Usage: shell-probe <socket-path> [event-count]
//!
//! Exits 0 after receiving `event-count` events (default 9 — three full
//! TierUpdate/StreamBudget/GearUpdate governor cycles), 1 if the daemon
//! goes silent for 3 s.

use std::path::PathBuf;
use std::time::Duration;

use lowband_platform::ipc::IpcClient;

fn main() {
    let socket = std::env::args().nth(1).unwrap_or_else(|| "/tmp/lowband.sock".into());
    let want: usize = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(9);

    let client = IpcClient::connect(&PathBuf::from(&socket)).unwrap_or_else(|e| {
        eprintln!("shell-probe: connect {socket}: {e}");
        std::process::exit(1);
    });
    eprintln!("shell-probe: connected to {socket}");

    for i in 0..want {
        match client.receiver().recv_timeout(Duration::from_secs(3)) {
            Ok(event) => println!("event[{i}]: {event:?}"),
            Err(e) => {
                eprintln!("shell-probe: no event within 3s ({e})");
                std::process::exit(1);
            }
        }
    }
    eprintln!("shell-probe: received {want} events, disconnecting");
}
