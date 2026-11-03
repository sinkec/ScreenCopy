//! Searches for Miracast receivers for a few seconds and prints them. Connects
//! to nothing.
//!
//!     cargo run --example p2p_search

use std::time::Duration;

use gtk::glib;
use monitorbeam::p2p::P2pClient;

const SEARCH_TIME: Duration = Duration::from_secs(10);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    glib::MainContext::default().block_on(async {
        let client = P2pClient::new().await?;
        client.start_search().await?;
        glib::timeout_future(SEARCH_TIME).await;
        let peers = client.peers().await?;
        client.stop_search().await?;
        println!("Found {} receiver(s)", peers.len());
        for peer in peers {
            println!(
                "{} [{}] signal {}%, {:?}",
                peer.name, peer.hw_address, peer.strength, peer.info
            );
        }
        Ok(())
    })
}
