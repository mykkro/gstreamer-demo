//! sim-streamer: renders a synthetic 3D scene headlessly with wgpu and streams several vehicle
//! cameras as H.265 channels, plus one "mosaic" channel with all cameras in one picture.
//!
//! ```text
//!  render thread (wgpu, headless)                     GStreamer (gst_demo::atlas)
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
use gst_demo::atlas::{AtlasArgs, AtlasStream};
use gst_demo::run_main_loop;

use render::Renderer;
use scene::{CAMERAS, Scene};

#[derive(Parser)]
#[command(about = "Headless 3D scene (raw wgpu) with several vehicle cameras, streamed as H.265 channels")]
struct Args {
    #[command(flatten)]
    atlas: AtlasArgs,
}

fn main() -> Result<()> {
    let Args { atlas: args } = Args::parse();
    gst::init()?;

    let mut renderer = Renderer::new(args.cam_width, args.cam_height, CAMERAS.len(), args.cols)?;
    let names: Vec<&str> = CAMERAS.iter().map(|c| c.name).collect();
    let source = format!("sim-streamer (wgpu): {} cameras, {}", CAMERAS.len(), renderer.adapter_info);
    let stream = AtlasStream::new(&args, &names, &source)?;
    let (width, height) = (stream.width, stream.height);

    // ---- render loop on its own thread ------------------------------------------------------
    let running = Arc::new(AtomicBool::new(true));
    let run = running.clone();
    let fps = args.fps;
    let appsrc = stream.appsrc.clone();
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
            if !AtlasStream::push_frame(&appsrc, data) {
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

    let result = run_main_loop(&stream.pipeline, |_| {});
    running.store(false, Ordering::Relaxed);
    if let Ok(Err(e)) = render_thread.join() {
        println!("render thread error: {e:#}");
    }
    result
}
