//! Video producer: decodes one source once, offers several H.265 channels.
//!
//! ```text
//! source (mp4, looped | test pattern)
//!     -> decode -> tee -+-> videorate -> [valve] scale -> H.265 enc -> MPEG-TS -> tcpserversink :5001
//!                       +-> ...                                                    tcpserversink :5002 ...
//! control server (JSON over TCP, :5000) answers "what channels do you have?"
//! ```
//!
//! Same pipeline and protocol as `python/producer.py`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Parser;
use gst::glib;
use gst::prelude::*;
use gst_demo::{
    Channel, DEFAULT_CONTROL_PORT, channel_branch, has_element, pick_encoder, run_main_loop, spawn_control_server,
    wire_channel,
};

#[derive(Parser)]
#[command(about = "GStreamer H.265 multi-channel producer")]
struct Args {
    /// Path to a video file (looped) or 'test'
    #[arg(default_value = "test")]
    source: String,
    /// Interface to listen on
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    control_port: u16,
    /// First channel port; channels use consecutive ports
    #[arg(long, default_value_t = 5001)]
    base_port: u16,
    /// Comma list of <height>p<fps>[:kbps]
    #[arg(long, default_value = "1080p30:4000,720p30:2000,480p15:600")]
    channels: String,
    /// auto | x265 | nvenc | qsv | amf | mf | <gst element name>
    #[arg(long, default_value = "auto")]
    encoder: String,
    /// Keyframe interval in seconds
    #[arg(long, default_value_t = 2)]
    gop_seconds: u32,
    /// Encode all channels even with no subscribers
    #[arg(long)]
    always_encode: bool,
}

/// "1080p30:4000,720p30" -> channels (16:9 width derived from height).
fn parse_channels(spec: &str, base_port: u16, encoder: &str) -> Result<Vec<Channel>> {
    spec.split(',')
        .enumerate()
        .map(|(i, item)| {
            let item = item.trim();
            let (res, kbps) = item.split_once(':').map_or((item, None), |(r, k)| (r, Some(k)));
            let (h, fps) = res
                .split_once('p')
                .with_context(|| format!("bad channel spec {item:?}, expected e.g. 720p30:2000"))?;
            let (height, fps): (u32, u32) = (h.parse()?, fps.parse()?);
            let width = ((height as f64 * 16.0 / 9.0 / 2.0).round() as u32) * 2;
            let bitrate_kbps = match kbps {
                Some(k) => k.parse()?,
                None => (width * height * fps / 15000).max(300),
            };
            Ok(Channel {
                id: format!("{height}p{fps}"),
                width,
                height,
                fps,
                bitrate_kbps,
                port: base_port + i as u16,
                codec: "h265".into(),
                container: "mpegts".into(),
                transport: "tcp".into(),
                encoder: encoder.into(),
            })
        })
        .collect()
}

fn pipeline_description(args: &Args, channels: &[Channel]) -> Result<String> {
    let source = if args.source == "test" {
        "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=1920,height=1080,framerate=30/1 ! \
         timeoverlay font-desc=\"Sans 36\" ! videoconvert name=srcconv"
            .to_string()
    } else {
        let path = std::fs::canonicalize(&args.source)?;
        let uri = glib::filename_to_uri(&path, None)?;
        // clocksync paces decoding to real time, even when no channel is consumed
        format!("uridecodebin uri=\"{uri}\" ! videoconvert name=srcconv ! clocksync")
    };
    let mut parts = vec![format!("{source} ! tee name=t allow-not-linked=true")];
    let overlay_ok = has_element("textoverlay");
    for ch in channels {
        let overlay = if overlay_ok {
            format!(
                "textoverlay text=\"{} {} (rust)\" valignment=top halignment=right font-desc=\"Sans 20\" ! ",
                ch.id, ch.encoder
            )
        } else {
            String::new()
        };
        // videorate sits before the valve so it never "fills" the closed-valve gap with duplicates
        let pre = format!(
            "videoscale ! video/x-raw,width={},height={},pixel-aspect-ratio=1/1 ! {overlay}",
            ch.width, ch.height
        );
        parts.push(format!(
            "t. ! queue max-size-buffers=3 leaky=downstream ! \
             videorate skip-to-first=true ! video/x-raw,framerate={}/1 ! {}",
            ch.fps,
            channel_branch(ch, &pre, &args.host)
        ));
    }
    Ok(parts.join(" "))
}

/// Seek the source (from the decoded side) to 0. With SEGMENT the demuxer posts SEGMENT_DONE
/// instead of EOS, and a non-flushing re-seek loops the file seamlessly with running timestamps.
fn seek_to_start(pipeline: &gst::Bin, flags: gst::SeekFlags) {
    let conv = pipeline.by_name("srcconv").unwrap();
    if conv
        .seek(1.0, flags, gst::SeekType::Set, gst::ClockTime::ZERO, gst::SeekType::None, gst::ClockTime::NONE)
        .is_err()
    {
        println!("warning: seek failed, file will not loop");
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.source != "test" && !Path::new(&args.source).is_file() {
        bail!("file not found: {}", args.source);
    }
    gst::init()?;

    let encoder = pick_encoder(&args.encoder)?;
    let channels = parse_channels(&args.channels, args.base_port, &encoder)?;
    let pipeline = gst::parse::launch(&pipeline_description(&args, &channels)?)?
        .downcast::<gst::Bin>()
        .unwrap();
    for ch in &channels {
        wire_channel(&pipeline, ch, args.gop_seconds, args.always_encode);
    }

    if args.source != "test" {
        // Once the first decoded frame arrives the demuxer exists -> start the looping segment seek
        let pipe = pipeline.downgrade();
        pipeline.by_name("srcconv").unwrap().static_pad("sink").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |_, _| {
                let pipe = pipe.clone();
                glib::idle_add_once(move || {
                    if let Some(p) = pipe.upgrade() {
                        seek_to_start(&p, gst::SeekFlags::FLUSH | gst::SeekFlags::SEGMENT);
                    }
                });
                gst::PadProbeReturn::Remove
            },
        );
    }

    let sinks = channels
        .iter()
        .map(|c| (c.id.clone(), pipeline.by_name(&format!("sink_{}", c.id)).unwrap()))
        .collect();
    spawn_control_server(&args.host, args.control_port, &args.source, &channels, sinks)?;

    println!("Source : {}", args.source);
    println!("Encoder: {encoder}");
    println!("Control: tcp://{}:{}", args.host, args.control_port);
    for ch in &channels {
        println!(
            "  {:>8}  {}x{}@{}  {} kbps  -> tcp port {}",
            ch.id, ch.width, ch.height, ch.fps, ch.bitrate_kbps, ch.port
        );
    }

    let pipe = pipeline.downgrade();
    run_main_loop(&pipeline, move |msg| {
        if let gst::MessageView::SegmentDone(_) = msg.view() {
            if let Some(p) = pipe.upgrade() {
                seek_to_start(&p, gst::SeekFlags::SEGMENT);
            }
        }
    })
}
