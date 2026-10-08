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

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use gst::glib;
use gst::prelude::*;
use gst_demo::{Channel, DEFAULT_CONTROL_PORT, has_element};
use serde_json::json;

/// Encoder preference for `--encoder auto`: hardware first, x265 (CPU) as fallback.
const AUTO_ENCODERS: &[&str] = &["nvh265enc", "qsvh265enc", "amfh265enc", "x265enc"];

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

fn pick_encoder(choice: &str) -> Result<String> {
    if choice == "auto" {
        return AUTO_ENCODERS
            .iter()
            .find(|n| has_element(n))
            .map(|n| n.to_string())
            .context("no H.265 encoder found");
    }
    let name = match choice {
        "x265" => "x265enc",
        "nvenc" => "nvh265enc",
        "qsv" => "qsvh265enc",
        "amf" => "amfh265enc",
        "mf" => "mfh265enc",
        other => other,
    };
    if !has_element(name) {
        bail!("encoder {name} is not available on this machine");
    }
    Ok(name.to_string())
}

/// Low-latency, CBR-ish settings per encoder family (values as strings so enums work by nick).
fn encoder_props(name: &str, ch: &Channel, gop: u32) -> Vec<(&'static str, String)> {
    let kbps = ch.bitrate_kbps.to_string();
    let gop = gop.to_string();
    match name {
        // ultrafast + zerolatency: no B-frames, no lookahead -> lowest delay, CPU-friendly
        "x265enc" => vec![
            ("speed-preset", "ultrafast".into()),
            ("tune", "zerolatency".into()),
            ("bitrate", kbps),
            ("key-int-max", gop),
        ],
        "nvh265enc" => vec![
            ("preset", "p4".into()),
            ("tune", "low-latency".into()),
            ("rc-mode", "cbr".into()),
            ("bitrate", kbps),
            ("gop-size", gop),
            ("zerolatency", "true".into()),
            ("repeat-sequence-header", "true".into()),
        ],
        "mfh265enc" => vec![
            ("rc-mode", "cbr".into()),
            ("bitrate", kbps),
            ("gop-size", gop),
            ("low-latency", "true".into()),
        ],
        _ => vec![("bitrate", kbps), ("gop-size", gop)],
    }
}

fn set_props(element: &gst::Element, props: &[(&str, String)]) {
    for (key, value) in props {
        if element.find_property(key).is_none() {
            continue;
        }
        // set_property_from_str panics on bad values; catch so one odd property doesn't kill us
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            element.set_property_from_str(key, value)
        }));
        if res.is_err() {
            println!("  (ignoring {}.{key}={value})", element.name());
        }
    }
}

fn pipeline_description(args: &Args, channels: &[Channel], encoder: &str) -> Result<String> {
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
        let id = &ch.id;
        let overlay = if overlay_ok {
            format!(
                "textoverlay text=\"{id} {encoder} (rust)\" valignment=top halignment=right font-desc=\"Sans 20\" ! "
            )
        } else {
            String::new()
        };
        parts.push(format!(
            "t. ! queue max-size-buffers=3 leaky=downstream ! \
             videorate skip-to-first=true ! video/x-raw,framerate={fps}/1 ! \
             valve name=valve_{id} drop=true ! videoscale ! \
             video/x-raw,width={w},height={h},pixel-aspect-ratio=1/1 ! \
             {overlay}videoconvert ! {encoder} name=enc_{id} ! \
             h265parse config-interval=-1 ! mpegtsmux alignment=7 ! \
             tcpserversink name=sink_{id} host={host} port={port} \
             sync=false async=false sync-method=next-keyframe recover-policy=keyframe",
            fps = ch.fps,
            w = ch.width,
            h = ch.height,
            host = args.host,
            port = ch.port,
        ));
    }
    Ok(parts.join(" "))
}

fn by_name(pipeline: &gst::Bin, name: &str) -> gst::Element {
    pipeline.by_name(name).unwrap_or_else(|| panic!("element {name} missing"))
}

/// Seek the source (from the decoded side) to 0. With SEGMENT the demuxer posts SEGMENT_DONE
/// instead of EOS, and a non-flushing re-seek loops the file seamlessly with running timestamps.
fn seek_to_start(pipeline: &gst::Bin, flags: gst::SeekFlags) {
    let conv = by_name(pipeline, "srcconv");
    if conv
        .seek(1.0, flags, gst::SeekType::Set, gst::ClockTime::ZERO, gst::SeekType::None, gst::ClockTime::NONE)
        .is_err()
    {
        println!("warning: seek failed, file will not loop");
    }
}

fn wire_channel(pipeline: &gst::Bin, ch: &Channel, encoder: &str, args: &Args) {
    let id = ch.id.clone();
    let enc = by_name(pipeline, &format!("enc_{id}"));
    let valve = by_name(pipeline, &format!("valve_{id}"));
    let sink = by_name(pipeline, &format!("sink_{id}"));
    set_props(&enc, &encoder_props(encoder, ch, ch.fps * args.gop_seconds));
    if args.always_encode {
        valve.set_property("drop", false);
    }

    let (v, cid) = (valve.clone(), id.clone());
    sink.connect("client-added", false, move |vals| {
        let sink = vals[0].get::<gst::Element>().unwrap();
        println!("[{cid}] subscriber connected ({} total)", sink.property::<u32>("num-handles"));
        v.set_property("drop", false);
        // Ask the encoder for an IDR frame right away so the new client starts fast
        let fku = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        enc.static_pad("src").unwrap().send_event(fku);
        None
    });

    let always = args.always_encode;
    sink.connect("client-socket-removed", false, move |vals| {
        let sink = vals[0].get::<gst::Element>().unwrap();
        let (valve, id) = (valve.clone(), id.clone());
        // num-handles is only updated after the signal returns
        glib::idle_add_once(move || {
            let n = sink.property::<u32>("num-handles");
            println!("[{id}] subscriber left ({n} remaining)");
            if n == 0 && !always {
                valve.set_property("drop", true);
            }
        });
        None
    });
}

/// Control server: one thread per connection, newline-delimited JSON.
fn serve_control(listener: TcpListener, info: serde_json::Value, sinks: Arc<Vec<(String, gst::Element)>>) {
    for stream in listener.incoming().flatten() {
        let (info, sinks) = (info.clone(), sinks.clone());
        std::thread::spawn(move || {
            let _ = handle_client(stream, &info, &sinks);
        });
    }
}

fn handle_client(stream: TcpStream, info: &serde_json::Value, sinks: &[(String, gst::Element)]) -> Result<()> {
    let mut out = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let reply = match serde_json::from_str::<serde_json::Value>(&line?) {
            Ok(msg) => match msg["cmd"].as_str() {
                Some("list" | "info") => info.clone(),
                Some("stats") => {
                    let clients: serde_json::Map<_, _> = sinks
                        .iter()
                        .map(|(id, s)| (id.clone(), json!(s.property::<u32>("num-handles"))))
                        .collect();
                    json!({ "ok": true, "clients": clients })
                }
                other => json!({ "ok": false, "error": format!("unknown command {other:?}") }),
            },
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        };
        writeln!(out, "{reply}")?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.source != "test" && !Path::new(&args.source).is_file() {
        bail!("file not found: {}", args.source);
    }
    gst::init()?;

    let encoder = pick_encoder(&args.encoder)?;
    let channels = parse_channels(&args.channels, args.base_port, &encoder)?;
    let pipeline = gst::parse::launch(&pipeline_description(&args, &channels, &encoder)?)?
        .downcast::<gst::Bin>()
        .unwrap();
    for ch in &channels {
        wire_channel(&pipeline, ch, &encoder, &args);
    }

    if args.source != "test" {
        // Once the first decoded frame arrives the demuxer exists -> start the looping segment seek
        let pipe = pipeline.downgrade();
        by_name(&pipeline, "srcconv").static_pad("sink").unwrap().add_probe(
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

    let main_loop = glib::MainLoop::new(None, false);
    let pipe = pipeline.downgrade();
    let ml = main_loop.clone();
    let _bus_watch = pipeline.bus().unwrap().add_watch(move |_, msg| {
        use gst::MessageView;
        match msg.view() {
            MessageView::SegmentDone(_) => {
                if let Some(p) = pipe.upgrade() {
                    seek_to_start(&p, gst::SeekFlags::SEGMENT);
                }
            }
            MessageView::Eos(_) => {
                println!("End of stream");
                ml.quit();
            }
            MessageView::Error(e) => {
                println!(
                    "ERROR from {}: {}\n  {:?}",
                    e.src().map(|s| s.name()).unwrap_or_default(),
                    e.error(),
                    e.debug()
                );
                ml.quit();
            }
            MessageView::Warning(w) => println!("WARNING: {}", w.error()),
            _ => {}
        }
        glib::ControlFlow::Continue
    })?;

    let info = json!({
        "ok": true,
        "source": args.source,
        "channels": channels,
    });
    let sinks: Arc<Vec<_>> = Arc::new(
        channels.iter().map(|c| (c.id.clone(), by_name(&pipeline, &format!("sink_{}", c.id)))).collect(),
    );
    let listener = TcpListener::bind((args.host.as_str(), args.control_port))
        .with_context(|| format!("cannot listen on control port {}", args.control_port))?;
    std::thread::spawn(move || serve_control(listener, info, sinks));

    println!("Source : {}", args.source);
    println!("Encoder: {encoder}");
    println!("Control: tcp://{}:{}", args.host, args.control_port);
    for ch in &channels {
        println!(
            "  {:>8}  {}x{}@{}  {} kbps  -> tcp port {}",
            ch.id, ch.width, ch.height, ch.fps, ch.bitrate_kbps, ch.port
        );
    }

    let ml = main_loop.clone();
    ctrlc::set_handler(move || ml.quit())?;
    pipeline.set_state(gst::State::Playing)?;
    main_loop.run();
    println!("Stopping...");
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}
