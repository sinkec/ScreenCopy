//! The source's side of a Wi-Fi Display session: negotiation M1 to M7, keepalive
//! and teardown, over one TCP connection from the TV.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{debug, info, warn};

use super::params::{
    VideoMode, choose_video_mode, parse_client_rtp_port, parse_parameters,
    parse_transport_client_port, supports_aac,
};
use super::rtsp::{self, Message, ParseError, StartLine};

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(30);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_TIMEOUT_SECONDS: u32 = 30;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const READ_CHUNK: usize = 4096;
/// Unparsed data is limited by the parser, plus one read that may follow it.
const MAX_BUFFERED: usize = 32 * 1024;

const STATUS_OK: u16 = 200;
const STATUS_NOT_SUPPORTED: u16 = 551;
const SERVER_RTP_PORT: u16 = 19000;
const PARAMETERS_URI: &str = "rtsp://localhost/wfd1.0";
const REQUEST_PARAMETERS: &str =
    "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n";
const PUBLIC_METHODS: &str =
    "Public: org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER";

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("the connection to the TV failed: {0}")]
    Io(#[from] io::Error),
    #[error("the TV sent an invalid message: {0}")]
    Parse(#[from] ParseError),
    #[error("the TV stopped responding")]
    Timeout,
    #[error("the TV closed the connection")]
    Closed,
    #[error("the TV refused {request} with status {status}")]
    Refused { request: &'static str, status: u16 },
    #[error("the TV did not send {0}")]
    Missing(&'static str),
    #[error("the TV supports no compatible video format")]
    NoVideoFormat,
    #[error("sending video failed: {0}")]
    Stream(String),
}

/// What to send and where, once the TV has asked to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTarget {
    pub address: Ipv4Addr,
    pub port: u16,
    pub local_port: u16,
    pub mode: VideoMode,
    pub with_audio: bool,
}

/// What the TV hears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// The computer's sound.
    System,
    /// Silence, because the computer's sound could not be captured.
    Silent,
    /// No audio at all, because the TV lacks a format this program can send.
    Absent,
}

/// Sends the media. Separate from the protocol so that it can be tested alone.
pub trait MediaStream {
    fn start(&mut self, target: StreamTarget) -> Result<Sound, String>;
    /// A failure that happened since the last call.
    fn poll_failure(&mut self) -> Option<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    Negotiating,
    Streaming(VideoMode, Sound),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Capabilities,
    Options,
    Configuration,
    TriggerSetup,
    Keepalive,
    TriggerTeardown,
}

impl Pending {
    fn request_name(self) -> &'static str {
        match self {
            Self::Options => "M1",
            Self::Capabilities => "M3",
            Self::Configuration => "M4",
            Self::TriggerSetup | Self::TriggerTeardown => "M5",
            Self::Keepalive => "M16",
        }
    }
}

struct Session<'a, M: MediaStream> {
    connection: TcpStream,
    buffer: Vec<u8>,
    media: &'a mut M,
    local_address: Ipv4Addr,
    peer_address: Ipv4Addr,
    session_id: u32,
    next_cseq: u32,
    pending: Option<(u32, Pending)>,
    options_answered: bool,
    sink_options_received: bool,
    mode: Option<VideoMode>,
    with_audio: bool,
    rtp_port: Option<u16>,
    streaming: bool,
    finished: bool,
    last_keepalive: Instant,
}

/// Runs a session until the TV ends it or `stop` is set. Ending because of
/// either of those is a success.
pub fn run(
    connection: TcpStream,
    local_address: Ipv4Addr,
    peer_address: Ipv4Addr,
    media: &mut impl MediaStream,
    stop: &AtomicBool,
    on_event: &mut impl FnMut(SessionEvent),
) -> Result<(), SessionError> {
    connection.set_read_timeout(Some(POLL_INTERVAL))?;
    connection.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut session = Session {
        connection,
        buffer: Vec::new(),
        media,
        local_address,
        peer_address,
        session_id: new_session_id(),
        next_cseq: 1,
        pending: None,
        options_answered: false,
        sink_options_received: false,
        mode: None,
        with_audio: false,
        rtp_port: None,
        streaming: false,
        finished: false,
        last_keepalive: Instant::now(),
    };
    on_event(SessionEvent::Negotiating);
    session.send_request(
        "OPTIONS",
        "*",
        &["Require: org.wfa.wfd1.0".to_owned()],
        "",
        Pending::Options,
    )?;

    let started = Instant::now();
    while !session.finished {
        if stop.load(Ordering::Relaxed) {
            return session.tear_down();
        }
        if !session.streaming && started.elapsed() > NEGOTIATION_TIMEOUT {
            return Err(SessionError::Timeout);
        }
        session.keep_alive()?;
        if let Some(failure) = session.media.poll_failure() {
            return Err(SessionError::Stream(failure));
        }
        if let Some(message) = session.read_message()? {
            session.handle(message, on_event)?;
        }
    }
    Ok(())
}

/// Identifies the session to the TV. It is not a secret: the connection is
/// already limited to the peer of the Wi-Fi Direct link.
fn new_session_id() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |time| time.subsec_nanos() | 1)
}

impl<M: MediaStream> Session<'_, M> {
    fn send(&mut self, text: &str) -> Result<(), SessionError> {
        debug!(message = %text, "sending to TV");
        self.connection.write_all(text.as_bytes())?;
        Ok(())
    }

    fn send_request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[String],
        body: &str,
        pending: Pending,
    ) -> Result<(), SessionError> {
        let cseq = self.next_cseq;
        self.next_cseq = self.next_cseq.wrapping_add(1);
        self.pending = Some((cseq, pending));
        self.send(&rtsp::format_request(method, uri, cseq, extra, body))
    }

    /// Returns one message, or `None` after a short wait without a whole one.
    fn read_message(&mut self) -> Result<Option<Message>, SessionError> {
        if let Some(message) = self.take_buffered()? {
            return Ok(Some(message));
        }
        let mut chunk = [0; READ_CHUNK];
        match self.connection.read(&mut chunk) {
            Ok(0) => Err(SessionError::Closed),
            Ok(count) => {
                let received = chunk.get(..count).unwrap_or_default();
                self.buffer.extend_from_slice(received);
                if self.buffer.len() > MAX_BUFFERED {
                    return Err(ParseError::HeadTooLong.into());
                }
                self.take_buffered()
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn take_buffered(&mut self) -> Result<Option<Message>, SessionError> {
        let Some((message, used)) = rtsp::parse(&self.buffer)? else {
            return Ok(None);
        };
        self.buffer.drain(..used);
        debug!(?message, "received from TV");
        Ok(Some(message))
    }

    fn handle(
        &mut self,
        message: Message,
        on_event: &mut impl FnMut(SessionEvent),
    ) -> Result<(), SessionError> {
        match message.start.clone() {
            StartLine::Response { status } => self.handle_response(&message, status),
            StartLine::Request { method, .. } => self.handle_request(&message, &method, on_event),
        }
    }

    fn handle_response(&mut self, message: &Message, status: u16) -> Result<(), SessionError> {
        let Some((cseq, pending)) = self.pending else {
            debug!("ignoring an unexpected response");
            return Ok(());
        };
        if message.cseq() != Some(cseq) {
            debug!("ignoring a response to an older request");
            return Ok(());
        }
        self.pending = None;
        if pending == Pending::Keepalive {
            return Ok(());
        }
        if status != STATUS_OK {
            return Err(SessionError::Refused {
                request: pending.request_name(),
                status,
            });
        }
        match pending {
            Pending::Options => {
                self.options_answered = true;
                self.request_capabilities_when_ready()
            }
            Pending::Capabilities => self.configure(&message.body),
            Pending::Configuration => self.trigger("SETUP", Pending::TriggerSetup),
            Pending::TriggerSetup | Pending::TriggerTeardown | Pending::Keepalive => Ok(()),
        }
    }

    /// M3 follows both our M1 being answered and the TV's own M2 being received.
    fn request_capabilities_when_ready(&mut self) -> Result<(), SessionError> {
        if !self.options_answered || !self.sink_options_received || self.mode.is_some() {
            return Ok(());
        }
        // Once only: M3 is pending or has been answered after this.
        self.options_answered = false;
        self.send_request(
            "GET_PARAMETER",
            PARAMETERS_URI,
            &[],
            REQUEST_PARAMETERS,
            Pending::Capabilities,
        )
    }

    fn configure(&mut self, body: &str) -> Result<(), SessionError> {
        let parameters = parse_parameters(body);
        let formats = parameters
            .get("wfd_video_formats")
            .ok_or(SessionError::Missing("its video formats"))?;
        let mode = choose_video_mode(formats).ok_or(SessionError::NoVideoFormat)?;
        info!(tv_formats = %formats, ?mode, "chose a video format");
        let port = parameters
            .get("wfd_client_rtp_ports")
            .and_then(|value| parse_client_rtp_port(value))
            .ok_or(SessionError::Missing("its video port"))?;
        self.with_audio = parameters
            .get("wfd_audio_codecs")
            .is_some_and(|value| supports_aac(value));
        self.mode = Some(mode);
        self.rtp_port = Some(port);

        let mut body = format!(
            "wfd_video_formats: {}\r\n\
             wfd_presentation_URL: rtsp://{}/wfd1.0/streamid=0 none\r\n\
             wfd_client_rtp_ports: RTP/AVP/UDP;unicast {port} 0 mode=play\r\n",
            mode.to_parameter(),
            self.local_address,
        );
        if self.with_audio {
            body.push_str("wfd_audio_codecs: AAC 00000001 00\r\n");
        }
        self.send_request(
            "SET_PARAMETER",
            PARAMETERS_URI,
            &[],
            &body,
            Pending::Configuration,
        )
    }

    fn trigger(&mut self, method: &str, pending: Pending) -> Result<(), SessionError> {
        let body = format!("wfd_trigger_method: {method}\r\n");
        self.send_request("SET_PARAMETER", PARAMETERS_URI, &[], &body, pending)
    }

    fn handle_request(
        &mut self,
        message: &Message,
        method: &str,
        on_event: &mut impl FnMut(SessionEvent),
    ) -> Result<(), SessionError> {
        let Some(cseq) = message.cseq() else {
            warn!(method, "ignoring a request without CSeq");
            return Ok(());
        };
        match method {
            "OPTIONS" => {
                self.send(&rtsp::format_ok(cseq, &[PUBLIC_METHODS.to_owned()]))?;
                self.sink_options_received = true;
                self.request_capabilities_when_ready()
            }
            "SETUP" => self.handle_setup(cseq, message),
            "PLAY" => self.handle_play(cseq, on_event),
            "TEARDOWN" => {
                info!("the TV ended the session");
                self.send(&rtsp::format_ok(cseq, &[self.session_header()]))?;
                self.finished = true;
                Ok(())
            }
            "GET_PARAMETER" | "SET_PARAMETER" | "PAUSE" => self.send(&rtsp::format_ok(cseq, &[])),
            _ => {
                debug!(method, "refusing an unknown request");
                self.send(&rtsp::format_error(
                    cseq,
                    STATUS_NOT_SUPPORTED,
                    "Option not supported",
                ))
            }
        }
    }

    fn session_header(&self) -> String {
        format!(
            "Session: {};timeout={SESSION_TIMEOUT_SECONDS}",
            self.session_id
        )
    }

    fn handle_setup(&mut self, cseq: u32, message: &Message) -> Result<(), SessionError> {
        if let Some(port) = message
            .header("Transport")
            .and_then(parse_transport_client_port)
        {
            self.rtp_port = Some(port);
        }
        let port = self
            .rtp_port
            .ok_or(SessionError::Missing("its video port"))?;
        let transport = format!(
            "Transport: RTP/AVP/UDP;unicast;client_port={port};server_port={SERVER_RTP_PORT}"
        );
        self.send(&rtsp::format_ok(cseq, &[self.session_header(), transport]))
    }

    fn handle_play(
        &mut self,
        cseq: u32,
        on_event: &mut impl FnMut(SessionEvent),
    ) -> Result<(), SessionError> {
        let (Some(mode), Some(port)) = (self.mode, self.rtp_port) else {
            return self.send(&rtsp::format_error(
                cseq,
                455,
                "Method not valid in this state",
            ));
        };
        self.send(&rtsp::format_ok(
            cseq,
            &[self.session_header(), "Range: npt=now-".to_owned()],
        ))?;
        if self.streaming {
            return Ok(());
        }
        let target = StreamTarget {
            address: self.peer_address,
            port,
            local_port: SERVER_RTP_PORT,
            mode,
            with_audio: self.with_audio,
        };
        info!(?target, "starting to send video");
        let sound = self.media.start(target).map_err(SessionError::Stream)?;
        self.streaming = true;
        self.last_keepalive = Instant::now();
        on_event(SessionEvent::Streaming(mode, sound));
        Ok(())
    }

    fn keep_alive(&mut self) -> Result<(), SessionError> {
        if !self.streaming || self.last_keepalive.elapsed() < KEEPALIVE_INTERVAL {
            return Ok(());
        }
        if matches!(self.pending, Some((_, Pending::Keepalive))) {
            return Err(SessionError::Timeout);
        }
        self.last_keepalive = Instant::now();
        let session = format!("Session: {}", self.session_id);
        self.send_request(
            "GET_PARAMETER",
            PARAMETERS_URI,
            &[session],
            "",
            Pending::Keepalive,
        )
    }

    /// Asks the TV to end the session, but does not depend on its cooperation.
    fn tear_down(&mut self) -> Result<(), SessionError> {
        if !self.streaming {
            return Ok(());
        }
        if let Err(error) = self.trigger("TEARDOWN", Pending::TriggerTeardown) {
            debug!(%error, "cannot ask the TV to end the session");
            return Ok(());
        }
        let started = Instant::now();
        while started.elapsed() < TEARDOWN_TIMEOUT {
            match self.read_message() {
                Ok(Some(message)) => {
                    if matches!(&message.start, StartLine::Request { method, .. } if method == "TEARDOWN")
                    {
                        if let Some(cseq) = message.cseq() {
                            let _ = self.send(&rtsp::format_ok(cseq, &[self.session_header()]));
                        }
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(_) => return Ok(()),
            }
        }
        debug!("the TV did not end the session in time");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    use super::*;

    const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;
    const FORMATS: &str = "00 00 01 04 00000080 00000000 00000000 00 0000 0000 00 none none";

    #[derive(Default)]
    struct FakeMedia {
        started: Vec<StreamTarget>,
        failure: Option<String>,
        start_failure: Option<String>,
    }

    impl MediaStream for FakeMedia {
        fn start(&mut self, target: StreamTarget) -> Result<Sound, String> {
            self.started.push(target);
            self.start_failure.clone().map_or(Ok(Sound::Absent), Err)
        }

        fn poll_failure(&mut self) -> Option<String> {
            if self.started.is_empty() {
                None
            } else {
                self.failure.take()
            }
        }
    }

    /// A scripted TV on the other end of a real TCP connection.
    struct FakeTv {
        reader: BufReader<TcpStream>,
        writer: TcpStream,
        cseq: u32,
    }

    impl FakeTv {
        fn read(&mut self) -> Message {
            self.reader
                .get_ref()
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut data = Vec::new();
            loop {
                if let Some((message, _)) = rtsp::parse(&data).unwrap() {
                    return message;
                }
                let mut byte = [0];
                let count = self.reader.read(&mut byte).unwrap();
                assert_eq!(count, 1, "connection closed");
                data.push(byte[0]);
            }
        }

        fn write(&mut self, text: &str) {
            self.writer.write_all(text.as_bytes()).unwrap();
        }

        fn reply(&mut self, request: &Message, body: &str) {
            let cseq = request.cseq().unwrap();
            let mut text = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n");
            if !body.is_empty() {
                text.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
            text.push_str("\r\n");
            text.push_str(body);
            self.write(&text);
        }

        fn request(&mut self, line: &str, extra: &str) -> Message {
            self.cseq += 1;
            let text = format!("{line} RTSP/1.0\r\nCSeq: {}\r\n{extra}\r\n", self.cseq);
            self.write(&text);
            self.read()
        }

        /// Plays M1 to M7 as a well-behaved TV.
        fn negotiate(&mut self, capabilities: &str) -> Vec<Message> {
            let mut seen = Vec::new();
            let m1 = self.read();
            self.reply(&m1, "");
            seen.push(m1);
            seen.push(self.request("OPTIONS *", "Require: org.wfa.wfd1.0\r\n"));
            let m3 = self.read();
            self.reply(&m3, capabilities);
            seen.push(m3);
            let m4 = self.read();
            self.reply(&m4, "");
            seen.push(m4);
            let m5 = self.read();
            self.reply(&m5, "");
            seen.push(m5);
            seen.push(self.request(
                "SETUP rtsp://x/wfd1.0/streamid=0",
                "Transport: RTP/AVP/UDP;unicast;client_port=2000\r\n",
            ));
            seen.push(self.request("PLAY rtsp://x/wfd1.0/streamid=0", ""));
            seen
        }
    }

    fn capabilities(audio: &str) -> String {
        format!(
            "wfd_video_formats: {FORMATS}\r\nwfd_audio_codecs: {audio}\r\n\
             wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play\r\n"
        )
    }

    struct Outcome {
        result: Result<(), SessionError>,
        media: FakeMedia,
        events: Vec<SessionEvent>,
    }

    /// Runs a session against `tv`, in a thread that plays the TV.
    fn run_with(
        media: FakeMedia,
        stop: Arc<AtomicBool>,
        tv: impl FnOnce(FakeTv) + Send + 'static,
    ) -> Outcome {
        let listener = TcpListener::bind((LOCAL, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let tv_thread = thread::spawn(move || {
            let writer = TcpStream::connect(address).unwrap();
            let reader = BufReader::new(writer.try_clone().unwrap());
            tv(FakeTv {
                reader,
                writer,
                cseq: 100,
            });
        });
        let (connection, _) = listener.accept().unwrap();
        let mut media = media;
        let mut events = Vec::new();
        let result = run(connection, LOCAL, LOCAL, &mut media, &stop, &mut |event| {
            events.push(event);
        });
        tv_thread.join().unwrap();
        Outcome {
            result,
            media,
            events,
        }
    }

    fn stop_flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn full_negotiation_starts_the_stream_and_tv_can_end_it() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            let seen = tv.negotiate(&capabilities("AAC 00000001 00"));

            assert_eq!(seen[0].header("Require"), Some("org.wfa.wfd1.0"));
            assert!(seen[1].header("Public").unwrap().contains("SETUP"));
            assert!(seen[2].body.contains("wfd_video_formats"));
            let m4 = &seen[3].body;
            assert!(
                m4.contains(&format!("wfd_video_formats: {FORMATS}")),
                "{m4}"
            );
            assert!(m4.contains("rtsp://127.0.0.1/wfd1.0/streamid=0"), "{m4}");
            assert!(m4.contains("wfd_audio_codecs: AAC"), "{m4}");
            assert!(seen[4].body.contains("wfd_trigger_method: SETUP"));
            let transport = seen[5].header("Transport").unwrap();
            assert!(transport.contains("client_port=2000"), "{transport}");
            assert!(transport.contains("server_port=19000"), "{transport}");
            assert!(seen[5].header("Session").unwrap().contains("timeout=30"));
            assert_eq!(seen[6].start, StartLine::Response { status: 200 });

            let reply = tv.request("TEARDOWN rtsp://x/wfd1.0/streamid=0", "");
            assert_eq!(reply.start, StartLine::Response { status: 200 });
        });

        outcome.result.unwrap();
        let target = outcome.media.started[0];
        assert_eq!(target.port, 2000);
        assert_eq!(target.mode.height, 1080);
        assert!(target.with_audio);
        assert_eq!(outcome.media.started.len(), 1);
        assert!(matches!(outcome.events[1], SessionEvent::Streaming(..)));
    }

    #[test]
    fn m2_may_arrive_before_the_answer_to_m1() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            let m1 = tv.read();
            tv.request("OPTIONS *", "");
            tv.reply(&m1, "");
            let m3 = tv.read();
            assert!(m3.body.contains("wfd_video_formats"));
        });
        assert!(matches!(outcome.result, Err(SessionError::Closed)));
    }

    #[test]
    fn video_without_audio_when_the_tv_has_no_aac() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            let seen = tv.negotiate(&capabilities("LPCM 00000002 00"));
            assert!(!seen[3].body.contains("wfd_audio_codecs"));
            tv.request("TEARDOWN x", "");
        });
        outcome.result.unwrap();
        assert!(!outcome.media.started[0].with_audio);
    }

    #[test]
    fn nothing_is_captured_before_play() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            let m1 = tv.read();
            tv.reply(&m1, "");
            tv.request("OPTIONS *", "");
            let m3 = tv.read();
            tv.reply(&m3, &capabilities("none"));
            let m4 = tv.read();
            tv.reply(&m4, "");
            let m5 = tv.read();
            tv.reply(&m5, "");
            tv.request("SETUP x", "Transport: RTP/AVP/UDP;client_port=2000\r\n");
        });
        assert!(matches!(outcome.result, Err(SessionError::Closed)));
        assert!(outcome.media.started.is_empty());
    }

    #[test]
    fn play_before_negotiation_is_refused() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            tv.read();
            let reply = tv.request("PLAY x", "");
            assert_eq!(reply.start, StartLine::Response { status: 455 });
        });
        assert!(matches!(outcome.result, Err(SessionError::Closed)));
        assert!(outcome.media.started.is_empty());
    }

    #[test]
    fn refused_configuration_is_reported() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            let m1 = tv.read();
            tv.reply(&m1, "");
            tv.request("OPTIONS *", "");
            let m3 = tv.read();
            tv.reply(&m3, &capabilities("none"));
            let m4 = tv.read();
            let cseq = m4.cseq().unwrap();
            tv.write(&format!("RTSP/1.0 400 Bad\r\nCSeq: {cseq}\r\n\r\n"));
            let _ = tv.reader.read(&mut [0]);
        });
        let error = outcome.result.unwrap_err();
        assert!(
            matches!(
                error,
                SessionError::Refused {
                    request: "M4",
                    status: 400
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn missing_or_unusable_capabilities_are_reported() {
        let cases = [
            ("wfd_audio_codecs: none\r\n", "video formats"),
            (
                "wfd_video_formats: none\r\nwfd_client_rtp_ports: RTP 1028 0\r\n",
                "video format",
            ),
            ("wfd_video_formats: 00 00 01 04 00000080\r\n", "video port"),
        ];
        for (body, expected) in cases {
            let outcome = run_with(FakeMedia::default(), stop_flag(), move |mut tv| {
                let m1 = tv.read();
                tv.reply(&m1, "");
                tv.request("OPTIONS *", "");
                let m3 = tv.read();
                tv.reply(&m3, body);
                let _ = tv.reader.read(&mut [0]);
            });
            let error = outcome.result.unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn garbage_from_the_tv_ends_the_session_with_an_error() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            tv.read();
            tv.write("this is not rtsp\r\n\r\n");
        });
        assert!(matches!(outcome.result, Err(SessionError::Parse(_))));
    }

    #[test]
    fn endless_data_from_the_tv_is_cut_off() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            tv.read();
            let junk = vec![b'x'; 64 * 1024];
            let _ = tv.writer.write_all(&junk);
        });
        assert!(matches!(outcome.result, Err(SessionError::Parse(_))));
    }

    #[test]
    fn unknown_request_is_refused_without_ending_the_session() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            tv.read();
            let reply = tv.request("RECORD x", "");
            assert_eq!(reply.start, StartLine::Response { status: 551 });
            let reply = tv.request("GET_PARAMETER x", "");
            assert_eq!(reply.start, StartLine::Response { status: 200 });
        });
        assert!(matches!(outcome.result, Err(SessionError::Closed)));
    }

    #[test]
    fn stop_asks_the_tv_to_end_the_session() {
        let stop = stop_flag();
        let stop_for_tv = Arc::clone(&stop);
        let outcome = run_with(FakeMedia::default(), stop, move |mut tv| {
            tv.negotiate(&capabilities("none"));
            stop_for_tv.store(true, Ordering::Relaxed);
            let trigger = tv.read();
            assert!(trigger.body.contains("wfd_trigger_method: TEARDOWN"));
            tv.reply(&trigger, "");
            let reply = tv.request("TEARDOWN x", "");
            assert_eq!(reply.start, StartLine::Response { status: 200 });
        });
        outcome.result.unwrap();
    }

    #[test]
    fn stop_does_not_depend_on_the_tv_cooperating() {
        let stop = stop_flag();
        let stop_for_tv = Arc::clone(&stop);
        let started = Instant::now();
        let outcome = run_with(FakeMedia::default(), stop, move |mut tv| {
            tv.negotiate(&capabilities("none"));
            stop_for_tv.store(true, Ordering::Relaxed);
            tv.read();
            thread::sleep(TEARDOWN_TIMEOUT + Duration::from_millis(500));
        });
        outcome.result.unwrap();
        assert!(started.elapsed() < NEGOTIATION_TIMEOUT);
    }

    #[test]
    fn stream_failure_ends_the_session() {
        let media = FakeMedia {
            failure: Some("encoder died".to_owned()),
            ..FakeMedia::default()
        };
        let outcome = run_with(media, stop_flag(), |mut tv| {
            tv.negotiate(&capabilities("none"));
            let _ = tv.reader.read(&mut [0]);
        });
        assert!(
            matches!(outcome.result, Err(SessionError::Stream(message)) if message == "encoder died")
        );
    }

    #[test]
    fn stream_that_cannot_start_ends_the_session() {
        let media = FakeMedia {
            start_failure: Some("no encoder".to_owned()),
            ..FakeMedia::default()
        };
        let outcome = run_with(media, stop_flag(), |mut tv| {
            tv.negotiate(&capabilities("none"));
            let _ = tv.reader.read(&mut [0]);
        });
        assert!(matches!(outcome.result, Err(SessionError::Stream(_))));
    }

    #[test]
    fn tv_disappearing_during_streaming_is_reported() {
        let outcome = run_with(FakeMedia::default(), stop_flag(), |mut tv| {
            tv.negotiate(&capabilities("none"));
        });
        assert!(matches!(outcome.result, Err(SessionError::Closed)));
        assert_eq!(outcome.media.started.len(), 1);
    }
}
