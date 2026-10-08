//! bevy-streamer: the sim-streamer idea with Bevy 0.18 as the engine.
//!
//! How rendering to a stream works in Bevy:
//! 1. Run Bevy **headless**: no window (`WindowPlugin { primary_window: None }`, `WinitPlugin` disabled)
//!    and a `ScheduleRunnerPlugin` loop at the stream frame rate.
//! 2. Create one **render-target `Image`** (the atlas) and give each camera
//!    `RenderTarget::Image(atlas)` plus a `Viewport` = its tile. This is Bevy's split-screen mechanism,
//!    aimed at a texture instead of a window.
//! 3. Spawn a **`Readback::texture(atlas)`** entity: Bevy copies the texture to the CPU every frame and
//!    triggers `ReadbackComplete` with the bytes, and an observer pushes them into GStreamer's `appsrc`.
//! 4. GStreamer (shared `gst_demo::atlas`, identical to sim-streamer) encodes the mosaic and per-camera
//!    crops as H.265 channels, with the same control protocol as every other producer here.

mod scene;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bevy::app::{ScheduleRunnerPlugin, TerminalCtrlCHandlerPlugin};
use bevy::camera::{RenderTarget, Viewport};
use bevy::pbr::{DistanceFog, FogFalloff};
use bevy::prelude::*;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_resource::{TextureFormat, TextureUsages};
use bevy::time::TimeUpdateStrategy;
use bevy::window::ExitCondition;
use bevy::winit::WinitPlugin;
use clap::Parser;
use gst_demo::atlas::{AtlasArgs, AtlasStream};
use gst_demo::run_main_loop;

use scene::CAMERAS;

#[derive(Parser)]
#[command(about = "Bevy 0.18 scene rendered headlessly, several vehicle cameras streamed as H.265 channels")]
struct Args {
    #[command(flatten)]
    atlas: AtlasArgs,
    /// Disable directional-light shadows (cheaper: every camera renders its own shadow cascades)
    #[arg(long)]
    no_shadows: bool,
}

const SKY: Color = Color::srgb(0.62, 0.76, 0.92);

/// Where read-back frames go, plus a few counters for the console statistics.
#[derive(Resource)]
struct StreamSink {
    appsrc: gst_app::AppSrc,
    cam_w: u32,
    cam_h: u32,
    cols: u32,
    atlas_size: UVec2,
    frames: AtomicU32,
    window_start: Instant,
    stopped: Arc<AtomicBool>,
}

fn main() -> anyhow::Result<()> {
    let Args { atlas: args, no_shadows } = Args::parse();
    gst::init()?;

    let names: Vec<&str> = CAMERAS.iter().map(|c| c.name).collect();
    let stream = AtlasStream::new(&args, &names, &format!("bevy-streamer (Bevy 0.18): {} cameras", CAMERAS.len()))?;

    // GStreamer's main loop (bus, encode-on-demand signals, Ctrl+C) runs on its own thread;
    // Bevy owns the main thread. When the GStreamer side stops, Bevy is told to exit.
    let stopped = Arc::new(AtomicBool::new(false));
    let (pipeline, stop) = (stream.pipeline.clone(), stopped.clone());
    let gst_thread = std::thread::spawn(move || {
        if let Err(e) = run_main_loop(&pipeline, |_| {}) {
            eprintln!("GStreamer: {e:#}");
        }
        stop.store(true, Ordering::Relaxed);
    });

    App::new()
        .insert_resource(ClearColor(SKY))
        .insert_resource(GlobalAmbientLight { color: Color::WHITE, brightness: 600.0, ..default() })
        // deterministic simulation time: every update advances exactly one video frame
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(1.0 / args.fps as f64)))
        .insert_resource(StreamSink {
            appsrc: stream.appsrc.clone(),
            cam_w: args.cam_width,
            cam_h: args.cam_height,
            cols: args.cols,
            atlas_size: UVec2::new(stream.width, stream.height),
            frames: AtomicU32::new(0),
            window_start: Instant::now(),
            stopped,
        })
        .insert_resource(Shadows(!no_shadows))
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin { primary_window: None, exit_condition: ExitCondition::DontExit, ..default() })
                .disable::<WinitPlugin>()
                // Ctrl+C is handled by the GStreamer thread (the ctrlc crate allows only one handler)
                .disable::<TerminalCtrlCHandlerPlugin>(),
        )
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / args.fps as f64)))
        .add_systems(Startup, setup)
        .add_systems(Update, (scene::drive, scene::fly, report_and_exit))
        .run();

    gst_thread.join().ok();
    Ok(())
}

#[derive(Resource)]
struct Shadows(bool);

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    sink: Res<StreamSink>,
    shadows: Res<Shadows>,
) {
    let ego = scene::spawn_world(&mut commands, &mut meshes, &mut materials, shadows.0);

    // The atlas all cameras render into. sRGB format: the bytes we read back are display-ready.
    let mut atlas = Image::new_target_texture(sink.atlas_size.x, sink.atlas_size.y, TextureFormat::Rgba8UnormSrgb, None);
    atlas.texture_descriptor.usage |= TextureUsages::COPY_SRC; // needed for the readback copy
    let atlas = images.add(atlas);

    // Six cameras as children of the ego car, each drawing into its own tile of the atlas.
    let aspect = sink.cam_w as f32 / sink.cam_h as f32;
    for (i, cam) in CAMERAS.iter().enumerate() {
        let tile = UVec2::new(i as u32 % sink.cols, i as u32 / sink.cols) * UVec2::new(sink.cam_w, sink.cam_h);
        let camera = commands
            .spawn((
                Camera3d::default(),
                Camera {
                    order: i as isize, // distinct order per camera sharing a target
                    viewport: Some(Viewport {
                        physical_position: tile,
                        physical_size: UVec2::new(sink.cam_w, sink.cam_h),
                        ..default()
                    }),
                    ..default()
                },
                RenderTarget::Image(atlas.clone().into()),
                Projection::Perspective(PerspectiveProjection {
                    fov: cam.vfov,
                    aspect_ratio: aspect,
                    near: 0.1,
                    far: 900.0,
                    ..default()
                }),
                DistanceFog { color: SKY, falloff: FogFalloff::Linear { start: 80.0, end: 380.0 }, ..default() },
                (cam.transform)(),
                Name::new(cam.name),
            ))
            .id();
        commands.entity(ego).add_child(camera);
    }

    // Read the atlas back every frame and hand it to GStreamer.
    commands.spawn(Readback::texture(atlas)).observe(on_readback);
}

fn on_readback(mut ev: On<ReadbackComplete>, sink: Res<StreamSink>) {
    let data = std::mem::take(&mut ev.event_mut().data);
    let (w, h) = (sink.atlas_size.x as usize, sink.atlas_size.y as usize);
    // Bevy pads rows to 256 bytes; with a camera width that is a multiple of 64 there is no padding
    let frame = if data.len() == w * h * 4 {
        data
    } else {
        let padded = data.len() / h;
        data.chunks_exact(padded).flat_map(|row| &row[..w * 4]).copied().collect()
    };
    if AtlasStream::push_frame(&sink.appsrc, frame) {
        sink.frames.fetch_add(1, Ordering::Relaxed);
    }
}

fn report_and_exit(mut sink: ResMut<StreamSink>, mut exit: MessageWriter<AppExit>) {
    if sink.stopped.load(Ordering::Relaxed) {
        exit.write(AppExit::Success);
        return;
    }
    let secs = sink.window_start.elapsed().as_secs_f64();
    if secs >= 5.0 {
        let n = sink.frames.swap(0, Ordering::Relaxed);
        println!("bevy: {:.1} frames/s read back and streamed ({} cameras)", n as f64 / secs, CAMERAS.len());
        sink.window_start = Instant::now();
    }
}
