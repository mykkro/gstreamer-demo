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
//!   -> {"cmd": "watch"}   <- the "list" reply (+ "watching": true), then one line per change:
//!                            {"event": "channel_added", "channel": {...}}
//!                            {"event": "channel_removed", "id": "..."}
//!   other commands (e.g. "add_camera") go to an application handler, see [`Control::set_handler`]

pub mod atlas;
pub mod streams;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
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

fn connect(host: &str, port: u16) -> Result<TcpStream> {
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))?
        .next()
        .context("cannot resolve producer address")?;
    Ok(TcpStream::connect_timeout(&addr, Duration::from_secs(3))?)
}

/// Send one JSON message to the producer's control port and return the reply.
pub fn request_json(host: &str, port: u16, msg: &serde_json::Value) -> Result<serde_json::Value> {
    let mut stream = connect(host, port)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(format!("{msg}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        bail!("producer closed control connection without reply");
    }
    Ok(serde_json::from_str(&line)?)
}

/// Send `{"cmd": cmd}` and return the reply.
pub fn request(host: &str, port: u16, cmd: &str) -> Result<serde_json::Value> {
    request_json(host, port, &json!({ "cmd": cmd }))
}

pub fn list_channels(host: &str, port: u16) -> Result<ChannelList> {
    Ok(serde_json::from_value(request(host, port, "list")?)?)
}

/// Subscribe to channel changes: calls `on_event` for every pushed event line until the connection
/// ends (returns Ok) or fails. Producers without `watch` support simply never send events.
pub fn watch(host: &str, port: u16, mut on_event: impl FnMut(&serde_json::Value)) -> Result<()> {
    let mut stream = connect(host, port)?;
    stream.write_all(b"{\"cmd\":\"watch\"}\n")?;
    let mut lines = BufReader::new(stream).lines();
    lines.next(); // the initial channel list
    for line in lines {
        if let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line?) {
            if ev.get("event").is_some() {
                on_event(&ev);
            }
        }
    }
    Ok(())
}

// ---- control protocol: server side ------------------------------------------------------------

type Handler = dyn Fn(&serde_json::Value) -> Option<serde_json::Value> + Send + Sync;

/// The control server's live state: channels can be added/removed at runtime (watchers are notified).
pub struct Control {
    source: String,
    inner: Mutex<ControlInner>,
}

struct ControlInner {
    /// channel + its tcpserversink (for subscriber counts)
    channels: Vec<(Channel, gst::Element)>,
    watchers: Vec<mpsc::Sender<String>>,
    handler: Option<Arc<Handler>>,
}

impl Control {
    fn info(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let channels: Vec<&Channel> = inner.channels.iter().map(|(c, _)| c).collect();
        json!({ "ok": true, "source": self.source, "channels": channels })
    }

    fn broadcast(inner: &mut ControlInner, event: serde_json::Value) {
        let line = event.to_string();
        inner.watchers.retain(|w| w.send(line.clone()).is_ok());
    }

    /// Publish a new channel (and tell all watchers).
    pub fn add_channel(&self, channel: Channel, sink: gst::Element) {
        let mut inner = self.inner.lock().unwrap();
        Self::broadcast(&mut inner, json!({ "event": "channel_added", "channel": channel }));
        inner.channels.push((channel, sink));
    }

    /// Withdraw a channel (and tell all watchers).
    pub fn remove_channel(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.channels.retain(|(c, _)| c.id != id);
        Self::broadcast(&mut inner, json!({ "event": "channel_removed", "id": id }));
    }

    /// Handle application commands (anything but list/stats/watch). Return `None` for "unknown command".
    /// Runs on the connection's thread; it may block (e.g. waiting for the engine to apply a change).
    pub fn set_handler(&self, handler: impl Fn(&serde_json::Value) -> Option<serde_json::Value> + Send + Sync + 'static) {
        self.inner.lock().unwrap().handler = Some(Arc::new(handler));
    }

    fn stats(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let clients: serde_json::Map<_, _> = inner
            .channels
            .iter()
            .map(|(c, s)| (c.id.clone(), json!(s.property::<u32>("num-handles"))))
            .collect();
        json!({ "ok": true, "clients": clients })
    }
}

/// Answer list / stats / watch (+ application commands) on `host:port` from background threads.
/// `sinks` are the channels' tcpserversinks (by channel id), used to report subscriber counts.
pub fn spawn_control_server(
    host: &str,
    port: u16,
    source: &str,
    channels: &[Channel],
    sinks: Vec<(String, gst::Element)>,
) -> Result<Arc<Control>> {
    let listener =
        TcpListener::bind((host, port)).with_context(|| format!("cannot listen on control port {port}"))?;
    let channels = channels
        .iter()
        .map(|c| {
            let sink = sinks.iter().find(|(id, _)| *id == c.id).map(|(_, s)| s.clone());
            (c.clone(), sink.unwrap_or_else(|| panic!("no sink for channel {}", c.id)))
        })
        .collect();
    let control = Arc::new(Control {
        source: source.into(),
        inner: Mutex::new(ControlInner { channels, watchers: vec![], handler: None }),
    });
    let ctl = control.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let ctl = ctl.clone();
            std::thread::spawn(move || {
                let _ = handle_control_client(stream, &ctl);
            });
        }
    });
    Ok(control)
}

fn handle_control_client(stream: TcpStream, control: &Control) -> Result<()> {
    let out = Arc::new(Mutex::new(stream.try_clone()?));
    let send = |v: &serde_json::Value| -> Result<()> { Ok(writeln!(out.lock().unwrap(), "{v}")?) };
    for line in BufReader::new(stream).lines() {
        let reply = match serde_json::from_str::<serde_json::Value>(&line?) {
            Ok(msg) => match msg["cmd"].as_str() {
                Some("list" | "info") => control.info(),
                Some("stats") => control.stats(),
                Some("watch") => {
                    // register first, so no change between the snapshot and the subscription is lost
                    let (tx, rx) = mpsc::channel::<String>();
                    control.inner.lock().unwrap().watchers.push(tx);
                    let mut info = control.info();
                    info["watching"] = json!(true);
                    send(&info)?;
                    let out = out.clone();
                    std::thread::spawn(move || {
                        for ev in rx {
                            if writeln!(out.lock().unwrap(), "{ev}").is_err() {
                                break; // client gone: dropping rx unregisters us on the next broadcast
                            }
                        }
                    });
                    continue;
                }
                _ => {
                    // clone the handler out of the lock: it may call back into add/remove_channel
                    let handler = control.inner.lock().unwrap().handler.clone();
                    handler
                        .and_then(|h| h(&msg))
                        .unwrap_or_else(|| json!({ "ok": false, "error": format!("unknown command {:?}", msg["cmd"]) }))
                }
            },
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        };
        send(&reply)?;
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
///
/// When the last subscriber leaves, the encoder element is also stopped (state NULL). Hardware
/// encoders keep their session (e.g. one of the ~8 NVENC sessions of a GeForce) as long as they are
/// running, even without input, so an idle-but-running channel would still use one up.
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

    let (v, e, cid) = (valve.clone(), enc.clone(), id.clone());
    sink.connect("client-added", false, move |vals| {
        let sink = vals[0].get::<gst::Element>().unwrap();
        println!("[{cid}] subscriber connected ({} total)", sink.property::<u32>("num-handles"));
        if e.is_locked_state() {
            // restart the encoder that was stopped when the channel went idle (it starts with an IDR)
            e.set_locked_state(false);
            let _ = e.sync_state_with_parent();
        }
        v.set_property("drop", false); // re-sends caps/segment to the (re)started encoder
        let fku = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        e.static_pad("src").unwrap().send_event(fku);
        None
    });

    sink.connect("client-socket-removed", false, move |vals| {
        let sink = vals[0].get::<gst::Element>().unwrap();
        let (valve, enc, id) = (valve.clone(), enc.clone(), id.clone());
        // num-handles is only updated after the signal returns
        glib::idle_add_once(move || {
            let n = sink.property::<u32>("num-handles");
            println!("[{id}] subscriber left ({n} remaining)");
            if n == 0 && !always_encode {
                valve.set_property("drop", true);
                // free the hardware encoder session; nothing flows into it while the valve is closed
                enc.set_locked_state(true);
                let _ = enc.set_state(gst::State::Null);
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
