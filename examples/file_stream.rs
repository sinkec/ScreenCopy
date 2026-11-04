//! Streams a video file to a UDP port on this computer, as to a TV. It lets you check
//! picture, sound, pausing and seeking without one, by saving what comes out.
//!
//!     cargo run --example file_stream -- film.mkv 29400 [subtitles.srt]
//!
//! Receive it with, for example:
//!
//!     gst-launch-1.0 udpsrc port=29400 caps=application/x-rtp,media=video,clock-rate=90000,encoding-name=MP2T \
//!         ! rtpmp2tdepay ! filesink location=out.ts
//!
//! Then type `p` to pause or go on, a number of seconds to seek, or `q` to quit.

use std::io::BufRead;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use monitorbeam::capture::file_stream::{FileStream, FilmQuality, Playback};
use monitorbeam::capture::recorder::Encoder;
use monitorbeam::subtitles::Subtitles;
use monitorbeam::wfd::params::choose_video_mode;
use monitorbeam::wfd::session::{MediaStream, StreamTarget};

const FORMATS_1080P30: &str = "00 00 01 04 00000080 00000000 00000000 00 0000 0000 00 none none";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gst::init()?;
    let mut arguments = std::env::args().skip(1);
    let path = PathBuf::from(arguments.next().ok_or("give a file")?);
    let port: u16 = arguments.next().ok_or("give a port")?.parse()?;
    let playback = Arc::new(Playback::new(path));
    if let Some(subtitles) = arguments.next() {
        playback.set_subtitles(Some(Arc::new(Subtitles::load(subtitles.as_ref())?)));
    }
    let encoder = Encoder::detect().ok_or("no encoder")?;
    let mut stream = FileStream::new(
        Arc::clone(&playback),
        encoder,
        FilmQuality::default(),
        Ipv4Addr::LOCALHOST,
        Arc::default(),
    );
    let mode = choose_video_mode(FORMATS_1080P30).ok_or("no format")?;
    stream.start(StreamTarget {
        address: Ipv4Addr::LOCALHOST,
        port,
        local_port: 0,
        mode,
        with_audio: true,
    })?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let status = playback.status();
        match line.trim() {
            "q" => break,
            "p" => playback.set_paused(!status.paused),
            seconds => {
                if let Ok(seconds) = seconds.parse::<u64>() {
                    playback.seek(seconds * 1000);
                }
            }
        }
        println!("{:?}", playback.status());
        if let Some(failure) = stream.poll_failure() {
            return Err(failure.into());
        }
    }
    Ok(())
}
