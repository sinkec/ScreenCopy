//! RTSP framing for Wi-Fi Display. Everything parsed here comes from the TV and
//! is untrusted: sizes are limited, and malformed input is an error, never a panic.

use thiserror::Error;

const MAX_HEAD_LENGTH: usize = 8 * 1024;
const MAX_BODY_LENGTH: usize = 16 * 1024;
const MAX_HEADERS: usize = 32;
const HEAD_END: &[u8] = b"\r\n\r\n";
const VERSION: &str = "RTSP/1.0";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("the message head is too long")]
    HeadTooLong,
    #[error("the message body is too long")]
    BodyTooLong,
    #[error("the message has too many headers")]
    TooManyHeaders,
    #[error("the message is not valid text")]
    NotText,
    #[error("malformed start line")]
    StartLine,
    #[error("malformed header")]
    Header,
    #[error("malformed Content-Length")]
    ContentLength,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartLine {
    Request { method: String, uri: String },
    Response { status: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub start: StartLine,
    headers: Vec<(String, String)>,
    pub body: String,
}

impl Message {
    /// The first header with this name, ignoring case as RTSP requires.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn cseq(&self) -> Option<u32> {
        self.header("CSeq")?.parse().ok()
    }
}

/// Parses one message from the start of `buffer`. Returns it with the number of
/// bytes it used, or `None` if more data is needed.
pub fn parse(buffer: &[u8]) -> Result<Option<(Message, usize)>, ParseError> {
    let Some(head_length) = buffer
        .windows(HEAD_END.len())
        .position(|window| window == HEAD_END)
    else {
        return if buffer.len() > MAX_HEAD_LENGTH {
            Err(ParseError::HeadTooLong)
        } else {
            Ok(None)
        };
    };
    if head_length > MAX_HEAD_LENGTH {
        return Err(ParseError::HeadTooLong);
    }
    let head = buffer.get(..head_length).ok_or(ParseError::HeadTooLong)?;
    let head = std::str::from_utf8(head).map_err(|_| ParseError::NotText)?;
    let mut lines = head.split("\r\n");
    let start = parse_start_line(lines.next().ok_or(ParseError::StartLine)?)?;

    let mut headers = Vec::new();
    for line in lines {
        if headers.len() == MAX_HEADERS {
            return Err(ParseError::TooManyHeaders);
        }
        let (name, value) = line.split_once(':').ok_or(ParseError::Header)?;
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(ParseError::Header);
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let mut message = Message {
        start,
        headers,
        body: String::new(),
    };

    let body_length = match message.header("Content-Length") {
        Some(value) => value.parse().map_err(|_| ParseError::ContentLength)?,
        None => 0,
    };
    if body_length > MAX_BODY_LENGTH {
        return Err(ParseError::BodyTooLong);
    }
    // Neither sum can overflow: both parts were limited above.
    let body_start = head_length + HEAD_END.len();
    let total = body_start + body_length;
    let Some(body) = buffer.get(body_start..total) else {
        return Ok(None);
    };
    message.body = std::str::from_utf8(body)
        .map_err(|_| ParseError::NotText)?
        .to_owned();
    Ok(Some((message, total)))
}

fn parse_start_line(line: &str) -> Result<StartLine, ParseError> {
    if let Some(rest) = line.strip_prefix(VERSION) {
        let mut parts = rest.trim_start().split(' ');
        let status = parts.next().ok_or(ParseError::StartLine)?;
        if !rest.starts_with(' ') || status.len() != 3 {
            return Err(ParseError::StartLine);
        }
        let status = status.parse().map_err(|_| ParseError::StartLine)?;
        return Ok(StartLine::Response { status });
    }
    let parts: Vec<&str> = line.split(' ').collect();
    let &[method, uri, version] = parts.as_slice() else {
        return Err(ParseError::StartLine);
    };
    if version != VERSION
        || method.is_empty()
        || uri.is_empty()
        || !method.bytes().all(is_token_byte)
    {
        return Err(ParseError::StartLine);
    }
    Ok(StartLine::Request {
        method: method.to_owned(),
        uri: uri.to_owned(),
    })
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// Writes a request. All values come from this program, never from the TV.
pub fn format_request(method: &str, uri: &str, cseq: u32, extra: &[String], body: &str) -> String {
    let mut text = format!("{method} {uri} {VERSION}\r\nCSeq: {cseq}\r\n");
    finish(&mut text, extra, body);
    text
}

pub fn format_ok(cseq: u32, extra: &[String]) -> String {
    let mut text = format!("{VERSION} 200 OK\r\nCSeq: {cseq}\r\n");
    finish(&mut text, extra, "");
    text
}

pub fn format_error(cseq: u32, status: u16, reason: &str) -> String {
    let mut text = format!("{VERSION} {status} {reason}\r\nCSeq: {cseq}\r\n");
    finish(&mut text, &[], "");
    text
}

fn finish(text: &mut String, extra: &[String], body: &str) {
    for header in extra {
        text.push_str(header);
        text.push_str("\r\n");
    }
    if !body.is_empty() {
        text.push_str("Content-Type: text/parameters\r\n");
        text.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    text.push_str("\r\n");
    text.push_str(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &[u8] = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nRequire: org.wfa.wfd1.0\r\n\r\n";
    const RESPONSE: &[u8] = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Type: text/parameters\r\n\
                              Content-Length: 5\r\n\r\nhello";

    #[test]
    fn parses_a_request() {
        let (message, used) = parse(REQUEST).unwrap().unwrap();
        assert_eq!(used, REQUEST.len());
        assert_eq!(
            message.start,
            StartLine::Request {
                method: "OPTIONS".to_owned(),
                uri: "*".to_owned()
            }
        );
        assert_eq!(message.cseq(), Some(1));
        assert_eq!(message.header("require"), Some("org.wfa.wfd1.0"));
    }

    #[test]
    fn parses_a_response_with_a_body() {
        let (message, used) = parse(RESPONSE).unwrap().unwrap();
        assert_eq!(used, RESPONSE.len());
        assert_eq!(message.start, StartLine::Response { status: 200 });
        assert_eq!(message.body, "hello");
    }

    #[test]
    fn leaves_following_data_alone() {
        let mut data = REQUEST.to_vec();
        data.extend_from_slice(RESPONSE);
        let (_, used) = parse(&data).unwrap().unwrap();
        let (second, _) = parse(&data[used..]).unwrap().unwrap();
        assert_eq!(second.start, StartLine::Response { status: 200 });
    }

    #[test]
    fn every_partial_message_asks_for_more() {
        for message in [REQUEST, RESPONSE] {
            for length in 0..message.len() {
                assert_eq!(parse(&message[..length]), Ok(None), "{length}");
            }
        }
    }

    #[test]
    fn rejects_endless_head() {
        let data = vec![b'a'; MAX_HEAD_LENGTH + 1];
        assert_eq!(parse(&data), Err(ParseError::HeadTooLong));
    }

    #[test]
    fn rejects_huge_declared_body_without_waiting_for_it() {
        let data = b"RTSP/1.0 200 OK\r\nContent-Length: 16385\r\n\r\n";
        assert_eq!(parse(data), Err(ParseError::BodyTooLong));
        let data = b"RTSP/1.0 200 OK\r\nContent-Length: 99999999999999999999999\r\n\r\n";
        assert_eq!(parse(data), Err(ParseError::ContentLength));
        let data = b"RTSP/1.0 200 OK\r\nContent-Length: -1\r\n\r\n";
        assert_eq!(parse(data), Err(ParseError::ContentLength));
    }

    #[test]
    fn rejects_too_many_headers() {
        let mut data = b"RTSP/1.0 200 OK\r\n".to_vec();
        for _ in 0..=MAX_HEADERS {
            data.extend_from_slice(b"A: b\r\n");
        }
        data.extend_from_slice(b"\r\n");
        assert_eq!(parse(&data), Err(ParseError::TooManyHeaders));
    }

    #[test]
    fn rejects_malformed_messages() {
        let cases: [(&[u8], ParseError); 9] = [
            (b"\r\n\r\n", ParseError::StartLine),
            (b"GARBAGE\r\n\r\n", ParseError::StartLine),
            (b"OPTIONS * HTTP/1.1\r\n\r\n", ParseError::StartLine),
            (b"RTSP/1.0\r\n\r\n", ParseError::StartLine),
            (b"RTSP/1.0 abc OK\r\n\r\n", ParseError::StartLine),
            (b"RTSP/1.00 200 OK\r\n\r\n", ParseError::StartLine),
            (b"OPTIONS * RTSP/1.0\r\nnocolon\r\n\r\n", ParseError::Header),
            (b"OPTIONS * RTSP/1.0\r\n: x\r\n\r\n", ParseError::Header),
            (b"\xFF\xFE * RTSP/1.0\r\n\r\n", ParseError::NotText),
        ];
        for (data, error) in cases {
            assert_eq!(parse(data), Err(error), "{data:?}");
        }
    }

    #[test]
    fn invalid_cseq_is_absent() {
        let (message, _) = parse(b"OPTIONS * RTSP/1.0\r\nCSeq: x\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(message.cseq(), None);
    }

    #[test]
    fn rejects_binary_body() {
        let data = b"RTSP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n\xFF\xFE";
        assert_eq!(parse(data), Err(ParseError::NotText));
    }

    #[test]
    fn formatted_messages_parse_back() {
        let text = format_request("SET_PARAMETER", "rtsp://x/", 7, &[], "a: b\r\n");
        let (message, used) = parse(text.as_bytes()).unwrap().unwrap();
        assert_eq!(used, text.len());
        assert_eq!(message.cseq(), Some(7));
        assert_eq!(message.body, "a: b\r\n");

        let text = format_ok(8, &["Session: 1".to_owned()]);
        let (message, _) = parse(text.as_bytes()).unwrap().unwrap();
        assert_eq!(message.header("Session"), Some("1"));

        let text = format_error(9, 551, "Option not supported");
        let (message, _) = parse(text.as_bytes()).unwrap().unwrap();
        assert_eq!(message.start, StartLine::Response { status: 551 });
    }

    /// A deterministic substitute for fuzzing: damaged real messages and random
    /// bytes must never panic, and a result must never claim more than it read.
    #[test]
    fn damaged_input_never_panics() {
        let mut state: u32 = 0x1234_5678;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as usize
        };
        for round in 0..60_000 {
            let mut data = if round % 2 == 0 { REQUEST } else { RESPONSE }.to_vec();
            for _ in 0..next() % 6 {
                let position = next() % data.len();
                data[position] = next().to_le_bytes()[0];
            }
            data.truncate(next() % (data.len() + 1));
            if let Ok(Some((_, used))) = parse(&data) {
                assert!(used <= data.len());
            }
        }
        for _ in 0..20_000 {
            let data: Vec<u8> = (0..next() % 128).map(|_| next().to_le_bytes()[0]).collect();
            let _ = parse(&data);
        }
    }
}
