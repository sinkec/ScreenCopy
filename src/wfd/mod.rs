//! Miracast: serves one TV over a Wi-Fi Direct link, from firewall to streaming.

pub mod params;
pub mod rtsp;
pub mod session;

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{debug, info};

use crate::capture::file_stream::{FileStream, FilmQuality, Playback};
use crate::capture::monitor::CaptureRegion;
use crate::capture::recorder::Encoder;
use crate::capture::stream::ScreenStream;
use crate::firewall::{FirewallError, FirewallSession};
use params::VideoPreference;
use session::{MediaStream, SessionError, SessionEvent};

const RTSP_PORT: u16 = 7236;
/// How long the TV may take to connect after the Wi-Fi Direct link is up.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// With no firewall known to be running, this long without the TV suggests one
/// that this program cannot see, such as hand-written iptables rules.
const ACCEPT_TIMEOUT_NOT_OPENED: Duration = Duration::from_secs(12);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The TV's connection and its address.
type AcceptedTv = (TcpStream, Ipv4Addr);

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("{0}")]
    Firewall(#[from] FirewallError),
    #[error("cannot listen for the TV: {0}")]
    Listen(io::Error),
    #[error("the TV did not connect in time, please try again")]
    AcceptTimeout,
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// What the TV is to show.
#[derive(Debug, Clone)]
pub enum MediaSource {
    Monitor(CaptureRegion),
    File(Arc<Playback>, FilmQuality),
}

#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub interface: String,
    pub local_address: Ipv4Addr,
    pub source: MediaSource,
    pub encoder: Encoder,
    pub sound_delay_ms: Arc<AtomicI32>,
    pub video_preference: VideoPreference,
    /// Whether a running firewall needs opening for the TV.
    pub open_firewall: bool,
}

#[derive(Debug)]
pub enum ServeEvent {
    Session(SessionEvent),
    /// The TV did not connect, so the firewall is being opened after all, which
    /// asks for the administrator password.
    OpeningFirewall,
    Ended(Result<(), String>),
}

/// Serves one session on the calling thread and reports through `events`. Sends
/// `Ended` last. A firewall that needed opening is open only for as long as this
/// runs.
pub fn serve(config: &ServeConfig, stop: &AtomicBool, events: &Sender<ServeEvent>) {
    let result = serve_session(config, stop, events).map_err(|error| error.to_string());
    if events.send(ServeEvent::Ended(result)).is_err() {
        debug!("nobody is waiting for the end of the session");
    }
}

fn serve_session(
    config: &ServeConfig,
    stop: &AtomicBool,
    events: &Sender<ServeEvent>,
) -> Result<(), ServeError> {
    let opened = config
        .open_firewall
        .then(|| FirewallSession::open(&config.interface))
        .transpose()?;

    // Only the Wi-Fi Direct address, never every interface.
    let listener =
        TcpListener::bind((config.local_address, RTSP_PORT)).map_err(ServeError::Listen)?;
    info!(address = %config.local_address, port = RTSP_PORT, "waiting for the TV");
    let (_firewall, accepted) = accept_opening_firewall(&listener, config, opened, stop, events)?;
    let Some((connection, peer_address)) = accepted else {
        return Ok(());
    };
    info!(%peer_address, "the TV connected");
    // One TV per session: nothing else may connect from here on.
    drop(listener);

    let accepted = (connection, peer_address);
    let result = match &config.source {
        MediaSource::Monitor(region) => {
            let mut media = ScreenStream::new(
                *region,
                config.encoder,
                config.local_address,
                Arc::clone(&config.sound_delay_ms),
            );
            run_session(accepted, config, &mut media, stop, events)
        }
        MediaSource::File(playback, quality) => {
            let mut media = FileStream::new(
                Arc::clone(playback),
                config.encoder,
                *quality,
                config.local_address,
                Arc::clone(&config.sound_delay_ms),
            );
            run_session(accepted, config, &mut media, stop, events)
        }
    };
    log_interface_traffic(&config.interface);
    Ok(result?)
}

fn run_session(
    (connection, peer_address): AcceptedTv,
    config: &ServeConfig,
    media: &mut impl MediaStream,
    stop: &AtomicBool,
    events: &Sender<ServeEvent>,
) -> Result<(), SessionError> {
    session::run(
        connection,
        config.local_address,
        peer_address,
        media,
        config.video_preference,
        stop,
        &mut |event| {
            if events.send(ServeEvent::Session(event)).is_err() {
                debug!("nobody is listening to session events");
            }
        },
    )
}

/// Waits for the TV. If the firewall was left closed and the TV does not come,
/// opens it and waits again, because something may be blocking the TV after all.
fn accept_opening_firewall(
    listener: &TcpListener,
    config: &ServeConfig,
    opened: Option<FirewallSession>,
    stop: &AtomicBool,
    events: &Sender<ServeEvent>,
) -> Result<(Option<FirewallSession>, Option<AcceptedTv>), ServeError> {
    if opened.is_some() {
        let accepted = accept_tv(listener, stop, ACCEPT_TIMEOUT).map_err(accept_error)?;
        return Ok((opened, accepted));
    }
    match accept_tv(listener, stop, ACCEPT_TIMEOUT_NOT_OPENED) {
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            info!("the TV did not connect, opening the firewall");
            if events.send(ServeEvent::OpeningFirewall).is_err() {
                debug!("nobody is listening to session events");
            }
            let firewall = FirewallSession::open(&config.interface)?;
            let accepted = accept_tv(listener, stop, ACCEPT_TIMEOUT).map_err(accept_error)?;
            Ok((Some(firewall), accepted))
        }
        accepted => Ok((None, accepted.map_err(accept_error)?)),
    }
}

/// Shows whether video really left the computer, when a TV stays black.
fn log_interface_traffic(interface: &str) {
    let read = |name: &str| {
        std::fs::read_to_string(format!("/sys/class/net/{interface}/statistics/{name}"))
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    info!(
        interface,
        sent_bytes = read("tx_bytes"),
        sent_packets = read("tx_packets"),
        received_bytes = read("rx_bytes"),
        "traffic during the session"
    );
}

/// Waits for the TV. Returns `None` if stopped first.
fn accept_tv(
    listener: &TcpListener,
    stop: &AtomicBool,
    timeout: Duration,
) -> Result<Option<AcceptedTv>, io::Error> {
    listener.set_nonblocking(true)?;
    let started = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match listener.accept() {
            Ok((connection, address)) => {
                connection.set_nonblocking(false)?;
                return Ok(Some((connection, peer_ipv4(address)?)));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() > timeout {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

fn accept_error(error: io::Error) -> ServeError {
    if error.kind() == io::ErrorKind::TimedOut {
        ServeError::AcceptTimeout
    } else {
        ServeError::Listen(error)
    }
}

fn peer_ipv4(address: SocketAddr) -> Result<Ipv4Addr, io::Error> {
    match address.ip() {
        IpAddr::V4(address) => Ok(address),
        IpAddr::V6(_) => Err(io::Error::from(io::ErrorKind::Unsupported)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_returns_the_connecting_peer() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let _client = TcpStream::connect(address).unwrap();
        let (_, peer) = accept_tv(&listener, &AtomicBool::new(false), ACCEPT_TIMEOUT)
            .unwrap()
            .unwrap();
        assert_eq!(peer, Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn waiting_for_the_tv_ends_after_the_given_time() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let error = accept_tv(&listener, &AtomicBool::new(false), Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn a_tv_that_never_connects_is_reported_as_such() {
        let error = accept_error(io::Error::from(io::ErrorKind::TimedOut));
        assert!(matches!(error, ServeError::AcceptTimeout), "{error}");
        let error = accept_error(io::Error::from(io::ErrorKind::AddrInUse));
        assert!(matches!(error, ServeError::Listen(_)), "{error}");
    }

    #[test]
    fn accept_can_be_stopped() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(
            accept_tv(&listener, &AtomicBool::new(true), ACCEPT_TIMEOUT)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn listener_binds_to_the_given_address_only() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert_eq!(
            listener.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }
}
