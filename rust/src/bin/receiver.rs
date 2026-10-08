//! Video receiver: asks the producer which channels exist, subscribes to one and displays it.
//!
//! ```text
//! control :5000  {"cmd":"list"}  ->  channel list (resolution, fps, bitrate, port)
//! video   :500x  tcpclientsrc -> tsdemux -> h265parse -> decodebin -> textoverlay -> video window
//! ```
//!
//! Unlike the Python receiver (which hands frames to OpenCV), this one uses GStreamer's own
//! video sink (D3D12/D3D11 on Windows). Keys in the window: 1..9 switch channel, q / Esc quit.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Parser;
use gst::prelude::*;
use gst_demo::{Channel, DEFAULT_CONTROL_PORT, has_element, list_channels};

#[derive(Parser)]
#[command(about = "GStreamer H.265 receiver")]
struct Args {
    /// Producer address
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    control_port: u16,
    /// Channel id to subscribe to, e.g. 720p30 (default: ask)
    #[arg(long)]
    channel: Option<String>,
    /// Only print the producer's channels and exit
    #[arg(long)]
    list: bool,
    /// auto | sw | d3d11 | d3d12 | nvdec | <gst element>
    #[arg(long, default_value = "auto")]
    decoder: String,
    /// Decode only, print fps (benchmark / headless)
    #[arg(long)]
    no_window: bool,
    /// Stop after N seconds (0 = run forever)
    #[arg(long, default_value_t = 0.0)]
    duration: f64,
}

enum Outcome {
    Quit,
    Switch(usize),
    Lost(String),
}

fn print_channels(source: &str, channels: &[Channel]) {
    println!("Producer source: {source}");
    for (i, ch) in channels.iter().enumerate() {
        println!(
            "  [{}] {:>8}  {}x{} @ {} fps  {}/{}  {} kbps  ({}, port {})",
            i + 1, ch.id, ch.width, ch.height, ch.fps, ch.codec, ch.container, ch.bitrate_kbps, ch.encoder, ch.port
        );
    }
}

fn choose_channel(channels: &[Channel], wanted: Option<&str>) -> Result<usize> {
    if let Some(w) = wanted {
        return match channels.iter().position(|c| c.id == w) {
            Some(i) => Ok(i),
            None => bail!(
                "channel {w:?} not offered. Available: {}",
                channels.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ")
            ),
        };
    }
    if std::io::stdin().is_terminal() {
        print!("Select channel [1-{}, default 1]: ", channels.len());
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans)?;
        if let Ok(n) = ans.trim().parse::<usize>() {
            if (1..=channels.len()).contains(&n) {
                return Ok(n - 1);
            }
        }
    }
    Ok(0)
}

fn build_pipeline(args: &Args, ch: &Channel) -> Result<gst::Bin> {
    let dec = match args.decoder.as_str() {
        "auto" => "decodebin", // picks the highest-ranked decoder (usually D3D12/D3D11 hardware)
        "sw" => "avdec_h265",
        "d3d11" => "d3d11h265dec",
        "d3d12" => "d3d12h265dec",
        "nvdec" => "nvh265dec",
        other => other,
    };
    if dec != "decodebin" && !has_element(dec) {
        bail!("decoder {dec} not available");
    }
    let tail = if args.no_window {
        "fakesink name=out sync=false".to_string()
    } else {
        format!(
            "videoconvert ! textoverlay name=out valignment=top halignment=left font-desc=\"Sans 14\" \
             text=\"{} - [1-9] switch, q quit\" ! autovideosink name=videosink sync=false",
            ch.id
        )
    };
    let desc = format!(
        "tcpclientsrc host={} port={} ! tsdemux latency=0 ! h265parse ! {dec} ! {tail}",
        args.host, ch.port
    );
    Ok(gst::parse::launch(&desc)?.downcast::<gst::Bin>().unwrap())
}

/// Map a key-press navigation message from the video window to an action.
fn key_action(msg: &gst::Message, current: usize, n_channels: usize) -> Option<Outcome> {
    let gst_video::NavigationMessage::Event(ev) = gst_video::NavigationMessage::parse(msg).ok()?;
    let gst_video::NavigationEvent::KeyPress { key, .. } = gst_video::NavigationEvent::parse(&ev.event).ok()?
    else {
        return None;
    };
    match key.as_str() {
        "q" | "Escape" => Some(Outcome::Quit),
        k => {
            let n: usize = k.parse().ok()?;
            (1..=n_channels).contains(&n).then_some(n - 1).filter(|&i| i != current).map(Outcome::Switch)
        }
    }
}

fn play(args: &Args, channels: &[Channel], idx: usize, stop: &AtomicBool, deadline: Option<Instant>) -> Result<Outcome> {
    let ch = &channels[idx];
    println!("Subscribing to {} on {}:{}", ch.id, args.host, ch.port);
    let pipeline = build_pipeline(args, ch)?;

    // Count frames arriving at the overlay/fakesink to measure the received frame rate
    let frames = Arc::new(AtomicU64::new(0));
    let out = pipeline.by_name("out").unwrap();
    let counter = frames.clone();
    // fakesink has a "sink" pad, textoverlay a "src" pad
    let pad = out.static_pad("src").or_else(|| out.static_pad("sink")).unwrap();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().unwrap();
    let mut last = (Instant::now(), 0u64);

    let outcome = loop {
        if stop.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() > d) {
            break Outcome::Quit;
        }
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => break Outcome::Lost("end of stream".into()),
                MessageView::Error(e) => {
                    // The D3D video sinks post an error when the user closes the window
                    let from_sink = msg.src().is_some_and(|s| s.path_string().contains("videosink"));
                    if from_sink {
                        break Outcome::Quit;
                    }
                    break Outcome::Lost(e.error().to_string());
                }
                MessageView::Element(_) => {
                    if let Some(o) = key_action(&msg, idx, channels.len()) {
                        break o;
                    }
                }
                _ => {}
            }
        }
        let (t0, n0) = last;
        if t0.elapsed() >= Duration::from_secs(1) {
            let n = frames.load(Ordering::Relaxed);
            let fps = (n - n0) as f64 / t0.elapsed().as_secs_f64();
            last = (Instant::now(), n);
            if args.no_window {
                println!("  {}: {}x{}  {fps:.1} fps", ch.id, ch.width, ch.height);
            } else {
                out.set_property(
                    "text",
                    format!("{}  {}x{}  {fps:.1} fps  [1-{}] switch, q quit", ch.id, ch.width, ch.height, channels.len()),
                );
            }
        }
    };
    let total = frames.load(Ordering::Relaxed);
    pipeline.set_state(gst::State::Null)?;
    if matches!(outcome, Outcome::Quit) {
        println!("Done: {total} frames received on {}", ch.id);
    }
    Ok(outcome)
}

fn main() -> Result<()> {
    let args = Args::parse();
    gst::init()?;

    let list = list_channels(&args.host, args.control_port).map_err(|e| {
        anyhow::anyhow!("cannot reach producer control port {}:{}: {e}", args.host, args.control_port)
    })?;
    print_channels(&list.source, &list.channels);
    if args.list {
        return Ok(());
    }
    let mut channels = list.channels;
    let mut idx = choose_channel(&channels, args.channel.as_deref())?;

    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    ctrlc::set_handler(move || s.store(true, Ordering::Relaxed))?;
    let deadline = (args.duration > 0.0).then(|| Instant::now() + Duration::from_secs_f64(args.duration));

    loop {
        match play(&args, &channels, idx, &stop, deadline)? {
            Outcome::Quit => return Ok(()),
            Outcome::Switch(i) => idx = i,
            Outcome::Lost(err) => {
                println!("Stream lost ({err}); reconnecting in 2 s...");
                std::thread::sleep(Duration::from_secs(2));
                // the producer may have restarted with different channels
                if let Ok(l) = list_channels(&args.host, args.control_port) {
                    channels = l.channels;
                    idx = idx.min(channels.len() - 1);
                }
            }
        }
    }
}
