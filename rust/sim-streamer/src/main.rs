//! sim-streamer: renders a synthetic 3D scene headlessly with wgpu and streams several vehicle
//! cameras as H.265 channels, plus one "mosaic" channel with all cameras in one picture.
//!
//! ```text
//!  render thread (wgpu, headless)                     GStreamer
//!  ┌──────────────────────────────┐   RGBA atlas   ┌──────────────────────────────────────────────┐
//!  │ 6 cameras -> 6 viewports of  │ ─────────────► │ appsrc ─ tee ─┬─ [valve] ─ H.265 ─ TCP :5001  mosaic
//!  │ one atlas texture, 1 readback│   per frame    │               ├─ [valve] crop ─ H.265 ─ :5002 front
//!  └──────────────────────────────┘                │               └─ ...                    :5007 top
//!                                                  │ control server :5000 (same protocol as producer)
//!                                                  └──────────────────────────────────────────────┘
//! ```
//!
//! Every existing receiver (Python, Rust, web) works unchanged: they just see more channels.

mod render;
mod scene;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use gst::prelude::*;
use gst_demo::{Channel, DEFAULT_CONTROL_PORT, channel_branch, pick_encoder, run_main_loop, spawn_control_server, wire_channel};

use render::Renderer;
use scene::{CAMERAS, Scene};

#[derive(Parser)]
#[command(about = "Headless 3D scene with several vehicle cameras, streamed as H.265 channels")]
struct Args {
    /// Interface to listen on
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    control_port: u16,
    /// First channel port (mosaic); cameras use the following ports
    #[arg(long, default_value_t = 5001)]
    base_port: u16,
    /// Width of one camera image (multiple of 64)
    #[arg(long, default_value_t = 640)]
    cam_width: u32,
    /// Height of one camera image
    #[arg(long, default_value_t = 360)]
    cam_height: u32,
    /// Cameras per atlas row
    #[arg(long, default_value_t = 3)]
    cols: u32,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// Bitrate per camera channel (kbps)
    #[arg(long, default_value_t = 1200)]
    cam_kbps: u32,
    /// Bitrate of the mosaic channel (kbps)
    #[arg(long, default_value_t = 5000)]
    mosaic_kbps: u32,
    /// auto | x265 | nvenc | qsv | amf | mf | <gst element name>
    #[arg(long, default_value = "auto")]
    encoder: String,
    #[arg(long, default_value_t = 1)]
    gop_seconds: u32,
    /// Encode all channels even with no subscribers
    #[arg(long)]
    always_encode: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    gst::init()?;

    let mut renderer = Renderer::new(args.cam_width, args.cam_height, CAMERAS.len(), args.cols)?;
    let (width, height) = renderer.atlas_size();
    let encoder = pick_encoder(&args.encoder)?;

    let mk = |i: usize, id: &str, w: u32, h: u32, kbps: u32| Channel {
        id: id.into(),
        width: w,
        height: h,
        fps: args.fps,
        bitrate_kbps: kbps,
        port: args.base_port + i as u16,
        codec: "h265".into(),
        container: "mpegts".into(),
        transport: "tcp".into(),
        encoder: encoder.clone(),
    };
    let mut channels = vec![mk(0, "mosaic", width, height, args.mosaic_kbps)];
    for (i, cam) in CAMERAS.iter().enumerate() {
        channels.push(mk(i + 1, cam.name, args.cam_width, args.cam_height, args.cam_kbps));
    }

    // One appsrc for the whole atlas; per-camera channels crop their tile after the valve,
    // so cropping and encoding only happen for channels somebody is watching.
    let mut desc = format!(
        "appsrc name=src format=time is-live=true do-timestamp=true \
         caps=video/x-raw,format=RGBA,width={width},height={height},framerate={}/1 ! \
         tee name=t allow-not-linked=true \
         t. ! queue max-size-buffers=2 leaky=downstream ! {}",
        args.fps,
        channel_branch(&channels[0], "", &args.host)
    );
    for (i, ch) in channels.iter().enumerate().skip(1) {
        let (x, y) = renderer.tile_origin(i - 1);
        let pre = format!(
            "videocrop left={x} top={y} right={} bottom={} ! \
             textoverlay text=\"{}\" valignment=top halignment=left font-desc=\"Sans 14\" ! ",
            width - x - args.cam_width,
            height - y - args.cam_height,
            ch.id
        );
        desc += &format!(" t. ! queue max-size-buffers=2 leaky=downstream ! {}", channel_branch(ch, &pre, &args.host));
    }
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Bin>().unwrap();
    for ch in &channels {
        wire_channel(&pipeline, ch, args.gop_seconds, args.always_encode);
    }
    let appsrc = pipeline.by_name("src").unwrap().downcast::<gst_app::AppSrc>().unwrap();
    // never block the render loop: keep at most 2 frames queued, drop the oldest
    appsrc.set_property_from_str("max-buffers", "2");
    appsrc.set_property_from_str("leaky-type", "downstream");

    let sinks = channels
        .iter()
        .map(|c| (c.id.clone(), pipeline.by_name(&format!("sink_{}", c.id)).unwrap()))
        .collect();
    let source = format!("sim-streamer: {} cameras, {}", CAMERAS.len(), renderer.adapter_info);
    spawn_control_server(&args.host, args.control_port, &source, &channels, sinks)?;

    println!("GPU    : {}", renderer.adapter_info);
    println!("Atlas  : {width}x{height} ({} cameras of {}x{}) @ {} fps", CAMERAS.len(), args.cam_width, args.cam_height, args.fps);
    println!("Encoder: {encoder}");
    println!("Control: tcp://{}:{}", args.host, args.control_port);
    for ch in &channels {
        println!("  {:>7}  {}x{}  {} kbps  -> tcp port {}", ch.id, ch.width, ch.height, ch.bitrate_kbps, ch.port);
    }

    // ---- render loop on its own thread ------------------------------------------------------
    let running = Arc::new(AtomicBool::new(true));
    let run = running.clone();
    let fps = args.fps;
    let render_thread = std::thread::spawn(move || -> Result<()> {
        let scene = Scene::new();
        let aspect = renderer.cam_w as f32 / renderer.cam_h as f32;
        let frame_time = Duration::from_secs_f64(1.0 / fps as f64);
        let start = Instant::now();
        let (mut frame, mut busy, mut window_start) = (0u64, Duration::ZERO, Instant::now());
        let mut window_frames = 0u32;
        while run.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            let t = frame as f32 / fps as f32; // simulation time: deterministic, independent of wall clock
            let (pos, dir) = scene.ego(t);
            let cams: Vec<_> = CAMERAS.iter().map(|c| c.view_proj(pos, dir, aspect)).collect();
            let mut data = vec![0u8; (width * height * 4) as usize];
            renderer.render(&cams, &scene.instances(t), &mut data)?;
            if appsrc.push_buffer(gst::Buffer::from_mut_slice(data)).is_err() {
                break; // pipeline is shutting down
            }
            busy += t0.elapsed();
            frame += 1;
            window_frames += 1;

            if window_start.elapsed() >= Duration::from_secs(5) {
                let secs = window_start.elapsed().as_secs_f64();
                println!(
                    "render: {:.1} fps, {:.1} ms/frame for {} cameras (render + readback + push)",
                    window_frames as f64 / secs,
                    busy.as_secs_f64() * 1000.0 / window_frames as f64,
                    CAMERAS.len()
                );
                (busy, window_frames, window_start) = (Duration::ZERO, 0, Instant::now());
            }
            // pace to real time; if we're late, don't sleep (and don't try to catch up)
            let due = start + frame_time * frame as u32;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        Ok(())
    });

    let result = run_main_loop(&pipeline, |_| {});
    running.store(false, Ordering::Relaxed);
    if let Ok(Err(e)) = render_thread.join() {
        println!("render thread error: {e:#}");
    }
    result
}
