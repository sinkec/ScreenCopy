//! Opens the firewall on an interface until Enter is pressed, to test the
//! helper and the polkit policy without a TV.
//!
//!     cargo run --example firewall_session -- p2p-test0

use std::io::BufRead as _;

use monitorbeam::firewall::FirewallSession;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let interface = std::env::args()
        .nth(1)
        .ok_or("usage: firewall_session <interface>")?;
    let session = FirewallSession::open(&interface)?;
    println!("Open on {interface}. Press Enter to close, or kill this program to test recovery.");
    std::io::stdin().lock().read_line(&mut String::new())?;
    session.close()?;
    println!("Closed.");
    Ok(())
}
