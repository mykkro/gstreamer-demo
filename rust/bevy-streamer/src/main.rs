//! bevy-streamer: a Bevy 0.18 scene with a camera rig, every camera streamed as its own H.265 channel.
//!
//! How rendering to a stream works in Bevy:
//! 1. Run Bevy **headless**: no window (`WindowPlugin { primary_window: None }`, `WinitPlugin` disabled)
//!    and a `ScheduleRunnerPlugin` loop at the highest camera frame rate.
//! 2. Give **each camera its own render-target `Image`** (`RenderTarget::Image`), so every camera can
//!    have its own resolution.
//! 3. Read each image back with a **`Readback::texture`** entity; an observer pushes the bytes into that
//!    camera's own GStreamer `appsrc` (shared `gst_demo::streams`).
//! 4. **Per-camera frame rate and on-demand rendering:** every tick a system decides per camera whether
//!    it renders this tick (`Camera::is_active`): only if someone watches its channel (or the mosaic) and
//!    only on its own rate (a 15 fps camera renders every 2nd tick of a 30 fps app). The camera's
//!    `Readback` is attached only on those ticks, so exactly the rendered frames are streamed.

mod scene;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bevy::app::{ScheduleRunnerPlugin, TerminalCtrlCHandlerPlugin};
use bevy::camera::RenderTarget;
use bevy::pbr::{DistanceFog, FogFalloff};
use bevy::prelude::*;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_resource::{TextureFormat, TextureUsages};
use bevy::time::TimeUpdateStrategy;
use bevy::window::ExitCondition;
use bevy::winit::WinitPlugin;
use clap::Parser;
use gst_demo::run_main_loop;
use gst_demo::streams::{CameraStreams, MosaicSpec, NetArgs, StreamSpec};

use scene::CAMERAS;

#[derive(Parser)]
#[command(about = "Bevy 0.18 scene rendered headlessly; each vehicle camera streamed as its own H.265 channel")]
struct Args {
    #[command(flatten)]
    net: NetArgs,
    /// Override a camera stream, e.g. --camera front=1920x1080@30:4000 --camera top=256x256@5
    /// (width must be a multiple of 64; kbps optional). Defaults: see `scene::CAMERAS`.
    #[arg(long = "camera", value_name = "NAME=WxH@FPS[:KBPS]")]
    cameras: Vec<String>,
    /// Don't offer the mosaic channel
    #[arg(long)]
    no_mosaic: bool,
    /// Mosaic tile size, e.g. 640x360
    #[arg(long, default_value = "640x360")]
    mosaic_tile: String,
    #[arg(long, default_value_t = 5000)]
    mosaic_kbps: u32,
    /// Disable directional-light shadows (cheaper: every camera renders its own shadow cascades)
    #[arg(long)]
    no_shadows: bool,
}

const SKY: Color = Color::srgb(0.62, 0.76, 0.92);

/// Per-camera bookkeeping, index = position in `CAMERAS`.
struct CamSlot {
    spec: StreamSpec,
    image: Handle<Image>,
    camera: Entity,
    readback: Entity,
    /// render every n-th app tick (app rate / camera rate)
    every: u64,
    pushed: Arc<AtomicU32>,
}

#[derive(Resource)]
struct Streaming {
    streams: CameraStreams,
    specs: Vec<StreamSpec>,
    slots: Vec<CamSlot>,
    tick: u64,
    window_start: Instant,
    stopped: Arc<AtomicBool>,
}

#[derive(Resource)]
struct Shadows(bool);

fn camera_specs(args: &Args) -> Result<Vec<StreamSpec>> {
    let mut specs: Vec<StreamSpec> =
        CAMERAS.iter().map(|c| StreamSpec::parse(c.name, c.stream)).collect::<Result<_>>()?;
    for o in &args.cameras {
        let (name, spec) = o.split_once('=').context("--camera expects NAME=WxH@FPS[:KBPS]")?;
        let slot = specs
            .iter_mut()
            .find(|s| s.name == name)
            .with_context(|| format!("unknown camera {name:?}; cameras: {}", CAMERAS.map(|c| c.name).join(", ")))?;
        *slot = StreamSpec::parse(name, spec)?;
    }
    Ok(specs)
}

fn main() -> Result<()> {
    let args = Args::parse();
    gst::init()?;

    let specs = camera_specs(&args)?;
    // the app ticks at the fastest camera's rate; slower cameras render every n-th tick
    let app_fps = specs.iter().map(|s| s.fps).max().unwrap();
    for s in &specs {
        anyhow::ensure!(app_fps % s.fps == 0, "camera {}: {} fps must divide the fastest rate ({app_fps} fps)", s.name, s.fps);
    }
    let mosaic = if args.no_mosaic {
        None
    } else {
        let (w, h) = args.mosaic_tile.split_once('x').context("--mosaic-tile expects WxH")?;
        Some(MosaicSpec { tile_w: w.parse()?, tile_h: h.parse()?, cols: 3, fps: app_fps, kbps: args.mosaic_kbps })
    };
    let streams = CameraStreams::new(
        &args.net,
        &specs,
        mosaic.as_ref(),
        &format!("bevy-streamer (Bevy 0.18): {} cameras, one image each", specs.len()),
    )?;

    // GStreamer's main loop (bus, encode-on-demand signals, Ctrl+C) runs on its own thread;
    // Bevy owns the main thread. When the GStreamer side stops, Bevy is told to exit.
    let stopped = Arc::new(AtomicBool::new(false));
    let (pipeline, stop) = (streams.pipeline.clone(), stopped.clone());
    let gst_thread = std::thread::spawn(move || {
        if let Err(e) = run_main_loop(&pipeline, |_| {}) {
            eprintln!("GStreamer: {e:#}");
        }
        stop.store(true, Ordering::Relaxed);
    });

    let frame = Duration::from_secs_f64(1.0 / app_fps as f64);
    App::new()
        .insert_resource(ClearColor(SKY))
        .insert_resource(GlobalAmbientLight { color: Color::WHITE, brightness: 600.0, ..default() })
        // deterministic simulation time: every update advances exactly one app tick
        .insert_resource(TimeUpdateStrategy::ManualDuration(frame))
        .insert_resource(Streaming { streams, specs, slots: vec![], tick: 0, window_start: Instant::now(), stopped })
        .insert_resource(Shadows(!args.no_shadows))
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin { primary_window: None, exit_condition: ExitCondition::DontExit, ..default() })
                .disable::<WinitPlugin>()
                // Ctrl+C is handled by the GStreamer thread (the ctrlc crate allows only one handler)
                .disable::<TerminalCtrlCHandlerPlugin>(),
        )
        .add_plugins(ScheduleRunnerPlugin::run_loop(frame))
        .add_systems(Startup, setup)
        .add_systems(Update, (scene::drive, scene::fly, schedule_cameras, report_and_exit))
        .run();

    gst_thread.join().ok();
    Ok(())
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut streaming: ResMut<Streaming>,
    shadows: Res<Shadows>,
) {
    let ego = scene::spawn_world(&mut commands, &mut meshes, &mut materials, shadows.0);
    let app_fps = streaming.specs.iter().map(|s| s.fps).max().unwrap();

    let mut slots = Vec::new();
    for (i, (cam, spec)) in CAMERAS.iter().zip(streaming.specs.clone()).enumerate() {
        // Each camera renders into its own image. sRGB format: the bytes we read back are display-ready.
        let mut image = Image::new_target_texture(spec.width, spec.height, TextureFormat::Rgba8UnormSrgb, None);
        image.texture_descriptor.usage |= TextureUsages::COPY_SRC; // needed for the readback copy
        let image = images.add(image);

        let camera = commands
            .spawn((
                Camera3d::default(),
                // starts inactive: schedule_cameras turns it on when someone is watching
                Camera { order: i as isize, is_active: false, ..default() },
                RenderTarget::Image(image.clone().into()),
                Projection::Perspective(PerspectiveProjection {
                    fov: cam.vfov,
                    aspect_ratio: spec.width as f32 / spec.height as f32,
                    near: 0.1,
                    far: 900.0,
                    ..default()
                }),
                DistanceFog { color: SKY, falloff: FogFalloff::Linear { start: 80.0, end: 380.0 }, ..default() },
                (cam.transform)(),
                Name::new(cam.name),
            ))
            .id();
        commands.entity(ego).add_child(camera); // the rig moves with the car

        // Readback entity for this camera. The Readback component itself is attached only on ticks
        // where the camera renders (see schedule_cameras); the observer forwards the bytes.
        let appsrc = streaming.streams.appsrcs[i].clone();
        let pushed = Arc::new(AtomicU32::new(0));
        let counter = pushed.clone();
        let (w, h) = (spec.width as usize, spec.height as usize);
        let readback = commands
            .spawn(Name::new(format!("readback {}", cam.name)))
            .observe(move |mut ev: On<ReadbackComplete>| {
                let data = std::mem::take(&mut ev.event_mut().data);
                // Bevy pads rows to 256 bytes; widths that are multiples of 64 have no padding
                let frame = if data.len() == w * h * 4 {
                    data
                } else {
                    let padded = data.len() / h;
                    data.chunks_exact(padded).flat_map(|row| &row[..w * 4]).copied().collect()
                };
                if CameraStreams::push_frame(&appsrc, frame) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            })
            .id();

        slots.push(CamSlot { every: (app_fps / spec.fps) as u64, spec, image, camera, readback, pushed });
    }
    streaming.slots = slots;
}

/// Per tick: render a camera only if its stream (or the mosaic) is watched and it is due at its rate.
fn schedule_cameras(mut streaming: ResMut<Streaming>, mut cameras: Query<&mut Camera>, mut commands: Commands) {
    if streaming.slots.is_empty() {
        return;
    }
    let demand = streaming.streams.demand();
    let tick = streaming.tick;
    streaming.tick += 1;
    for (slot, wanted) in streaming.slots.iter().zip(demand) {
        let active = wanted && tick % slot.every == 0;
        if let Ok(mut cam) = cameras.get_mut(slot.camera) {
            if cam.is_active != active {
                cam.is_active = active;
            }
        }
        if active {
            commands.entity(slot.readback).insert(Readback::texture(slot.image.clone()));
        } else {
            commands.entity(slot.readback).remove::<Readback>();
        }
    }
}

fn report_and_exit(mut streaming: ResMut<Streaming>, mut exit: MessageWriter<AppExit>) {
    if streaming.stopped.load(Ordering::Relaxed) {
        exit.write(AppExit::Success);
        return;
    }
    let secs = streaming.window_start.elapsed().as_secs_f64();
    if secs >= 5.0 {
        let report: Vec<String> = streaming
            .slots
            .iter()
            .map(|s| {
                let fps = s.pushed.swap(0, Ordering::Relaxed) as f64 / secs;
                if fps > 0.0 { format!("{} {:.1}/{}", s.spec.name, fps, s.spec.fps) } else { format!("{} idle", s.spec.name) }
            })
            .collect();
        println!("bevy: rendered+streamed fps: {}", report.join(" | "));
        streaming.window_start = Instant::now();
    }
}
