//! Shared bits of the demo, used by `producer`, `receiver` and `sim-streamer`:
//!
//! * the JSON control protocol (identical to the Python version, so Rust and Python
//!   producers/receivers can be mixed freely)
//! * H.265 encoder selection/settings and the per-channel "encode on demand" wiring
//! * [`atlas`]: the engine-agnostic "RGBA atlas of N cameras -> H.265 channels" streamer
//!
//! Protocol: newline-delimited JSON over TCP.
//!   -> {"cmd": "list"}    <- {"ok": true, "source": ..., "channels": [...]}
//!   -> {"cmd": "stats"}   <- {"ok": true, "clients": {"720p30": 2, ...}}

pub mod atlas;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gst::glib;
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const DEFAULT_CONTROL_PORT: u16 = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub port: u16,
    pub codec: String,
    pub container: String,
    pub transport: String,
    pub encoder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelList {
    pub ok: bool,
    pub source: String,
    pub channels: Vec<Channel>,
}

// ---- control protocol: client side ------------------------------------------------------------

/// Send one JSON command to the producer's control port and return the raw reply.
pub fn request(host: &str, port: u16, cmd: &str) -> Result<serde_json::Value> {
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))?
        .next()
        .context("cannot resolve producer address")?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(format!("{}\n", json!({ "cmd": cmd })).as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        bail!("producer closed control connection without reply");
    }
    Ok(serde_json::from_str(&line)?)
}

pub fn list_channels(host: &str, port: u16) -> Result<ChannelList> {
    Ok(serde_json::from_value(request(host, port, "list")?)?)
}

// ---- control protocol: server side ------------------------------------------------------------

/// Answer `list` / `stats` on `host:port` from a background thread (one thread per connection).
/// `sinks` are the channels' tcpserversinks, used to report subscriber counts.
pub fn spawn_control_server(
    host: &str,
    port: u16,
    source: &str,
    channels: &[Channel],
    sinks: Vec<(String, gst::Element)>,
) -> Result<()> {
    let listener =
        TcpListener::bind((host, port)).with_context(|| format!("cannot listen on control port {port}"))?;
    let info = json!({ "ok": true, "source": source, "channels": channels });
    let sinks = std::sync::Arc::new(sinks);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (info, sinks) = (info.clone(), sinks.clone());
            std::thread::spawn(move || {
                let _ = handle_control_client(stream, &info, &sinks);
            });
        }
    });
    Ok(())
}

fn handle_control_client(stream: TcpStream, info: &serde_json::Value, sinks: &[(String, gst::Element)]) -> Result<()> {
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

// ---- encoders ---------------------------------------------------------------------------------

pub fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Encoder preference for `--encoder auto`: hardware first, x265 (CPU) as fallback.
const AUTO_ENCODERS: &[&str] = &["nvh265enc", "qsvh265enc", "amfh265enc", "x265enc"];

/// `auto | x265 | nvenc | qsv | amf | mf | <element name>` -> available GStreamer element name.
pub fn pick_encoder(choice: &str) -> Result<String> {
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
pub fn encoder_props(name: &str, kbps: u32, gop: u32) -> Vec<(&'static str, String)> {
    let (kbps, gop) = (kbps.to_string(), gop.to_string());
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

// ---- per-channel output branch ----------------------------------------------------------------

/// gst-launch fragment for the end of a channel branch: valve -> `pre` -> encoder -> MPEG-TS -> TCP.
/// Element names follow the convention `valve_<id>`, `enc_<id>`, `sink_<id>` used by [`wire_channel`].
pub fn channel_branch(ch: &Channel, pre: &str, host: &str) -> String {
    let id = &ch.id;
    format!(
        "valve name=valve_{id} drop=true ! {pre} videoconvert ! {enc} name=enc_{id} ! \
         h265parse config-interval=-1 ! mpegtsmux alignment=7 ! \
         tcpserversink name=sink_{id} host={host} port={port} \
         sync=false async=false sync-method=next-keyframe recover-policy=keyframe",
        enc = ch.encoder,
        port = ch.port,
    )
}

/// Configure a channel built with [`channel_branch`]: encoder settings, and "encode on demand":
/// the valve opens when the first subscriber connects (and an IDR frame is requested so the
/// subscriber starts immediately) and closes again when the last one leaves.
pub fn wire_channel(pipeline: &gst::Bin, ch: &Channel, gop_seconds: u32, always_encode: bool) {
    let by_name = |n: String| pipeline.by_name(&n).unwrap_or_else(|| panic!("element {n} missing"));
    let id = ch.id.clone();
    let enc = by_name(format!("enc_{id}"));
    let valve = by_name(format!("valve_{id}"));
    let sink = by_name(format!("sink_{id}"));
    set_props(&enc, &encoder_props(&ch.encoder, ch.bitrate_kbps, ch.fps * gop_seconds));
    if always_encode {
        valve.set_property("drop", false);
    }

    let (v, cid) = (valve.clone(), id.clone());
    sink.connect("client-added", false, move |vals| {
        let sink = vals[0].get::<gst::Element>().unwrap();
        println!("[{cid}] subscriber connected ({} total)", sink.property::<u32>("num-handles"));
        v.set_property("drop", false);
        let fku = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        enc.static_pad("src").unwrap().send_event(fku);
        None
    });

    sink.connect("client-socket-removed", false, move |vals| {
        let sink = vals[0].get::<gst::Element>().unwrap();
        let (valve, id) = (valve.clone(), id.clone());
        // num-handles is only updated after the signal returns
        glib::idle_add_once(move || {
            let n = sink.property::<u32>("num-handles");
            println!("[{id}] subscriber left ({n} remaining)");
            if n == 0 && !always_encode {
                valve.set_property("drop", true);
            }
        });
        None
    });
}

/// Run a GLib main loop until Ctrl+C, EOS or an error. `on_message` sees every bus message first.
pub fn run_main_loop(pipeline: &gst::Bin, mut on_message: impl FnMut(&gst::Message) + Send + 'static) -> Result<()> {
    let main_loop = glib::MainLoop::new(None, false);
    let ml = main_loop.clone();
    let _watch = pipeline.bus().unwrap().add_watch(move |_, msg| {
        on_message(msg);
        match msg.view() {
            gst::MessageView::Eos(_) => {
                println!("End of stream");
                ml.quit();
            }
            gst::MessageView::Error(e) => {
                println!(
                    "ERROR from {}: {}\n  {:?}",
                    e.src().map(|s| s.name()).unwrap_or_default(),
                    e.error(),
                    e.debug()
                );
                ml.quit();
            }
            gst::MessageView::Warning(w) => println!("WARNING: {}", w.error()),
            _ => {}
        }
        glib::ControlFlow::Continue
    })?;
    let ml = main_loop.clone();
    ctrlc::set_handler(move || ml.quit())?;
    pipeline.set_state(gst::State::Playing)?;
    main_loop.run();
    println!("Stopping...");
    pipeline.set_state(gst::State::Null)?;
    Ok(())
}
