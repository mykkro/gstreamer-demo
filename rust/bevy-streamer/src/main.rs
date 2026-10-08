//! bevy-streamer: a Bevy 0.18 scene whose cameras are streamed as H.265 channels; cameras can be
//! added and removed at runtime (control protocol `add_camera` / `remove_camera`, or scripted).
//!
//! How rendering to a stream works in Bevy:
//! 1. Run Bevy **headless**: no window (`WindowPlugin { primary_window: None }`, `WinitPlugin` disabled)
//!    and a `ScheduleRunnerPlugin` loop at a fixed tick rate.
//! 2. Give **each camera its own render-target `Image`** (`RenderTarget::Image`), so every camera can
//!    have its own resolution.
//! 3. Read each image back with a **`Readback::texture`** entity; an observer pushes the bytes into that
//!    camera's own GStreamer `appsrc` (shared `gst_demo::streams`).
//! 4. **Per-camera frame rate and on-demand rendering:** every tick `schedule_cameras` decides per camera
//!    whether it renders (`Camera::is_active`): only if someone watches its channel (or the mosaic) and
//!    only on its own rate. The `Readback` is attached only on those ticks.
//! 5. **Dynamic cameras:** the control server's thread sends add/remove commands through a channel;
//!    `apply_commands` creates/destroys the camera, its image, readback and GStreamer branch, and the
//!    control server pushes `channel_added` / `channel_removed` to watching clients.

mod cameras;
mod scene;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bevy::app::{ScheduleRunnerPlugin, TerminalCtrlCHandlerPlugin};
use bevy::prelude::*;
use bevy::render::gpu_readback::Readback;
use bevy::time::TimeUpdateStrategy;
use bevy::window::ExitCondition;
use bevy::winit::WinitPlugin;
use clap::Parser;
use gst_demo::run_main_loop;
use gst_demo::streams::{CameraStreams, MosaicSpec, NetArgs, StreamSpec};
use serde_json::json;

use cameras::{CameraRequest, Cameras, Command, DroneDirector, Placement, SKY};
use scene::CAMERAS;

#[derive(Parser)]
#[command(about = "Bevy 0.18 scene rendered headlessly; cameras streamed as H.265 channels, added/removed at runtime")]
struct Args {
    #[command(flatten)]
    net: NetArgs,
    /// Override a startup camera, e.g. --camera front=1920x1080@30:4000 --camera top=256x256@5
    /// (width must be a multiple of 64; fps must divide --tick-fps; kbps optional)
    #[arg(long = "camera", value_name = "NAME=WxH@FPS[:KBPS]")]
    cameras: Vec<String>,
    /// Engine tick rate; every camera's fps must divide it
    #[arg(long, default_value_t = 30)]
    tick_fps: u32,
    /// Maximum number of cameras (also bounded by hardware encoder sessions, see README)
    #[arg(long, default_value_t = 9)]
    max_cameras: usize,
    /// Don't offer the mosaic channel
    #[arg(long)]
    no_mosaic: bool,
    /// Mosaic grid (fixed; cameras take free slots), e.g. 3x3
    #[arg(long, default_value = "3x3")]
    mosaic_grid: String,
    /// Mosaic tile size, e.g. 640x360
    #[arg(long, default_value = "640x360")]
    mosaic_tile: String,
    #[arg(long, default_value_t = 6000)]
    mosaic_kbps: u32,
    /// Seconds between scripted drone cameras (0 = off)
    #[arg(long, default_value_t = 30.0)]
    drone_cam_every: f32,
    /// Lifetime of a scripted drone camera (seconds)
    #[arg(long, default_value_t = 20.0)]
    drone_cam_lifetime: f32,
    /// Disable directional-light shadows (cheaper: every camera renders its own shadow cascades)
    #[arg(long)]
    no_shadows: bool,
}

#[derive(Resource)]
struct Settings {
    shadows: bool,
    startup: Vec<CameraRequest>,
}

#[derive(Resource)]
struct Runtime {
    tick: u64,
    window_start: Instant,
    stopped: Arc<AtomicBool>,
}

fn parse_pair(s: &str, what: &str) -> Result<(u32, u32)> {
    let (a, b) = s.split_once('x').with_context(|| format!("{what} expects AxB, got {s:?}"))?;
    Ok((a.parse()?, b.parse()?))
}

fn startup_requests(args: &Args) -> Result<Vec<CameraRequest>> {
    let mut reqs: Vec<CameraRequest> = CAMERAS
        .iter()
        .map(|c| {
            Ok(CameraRequest {
                spec: StreamSpec::parse(c.name, c.stream)?,
                placement: Placement::Rig((c.transform)()),
                vfov: c.vfov,
                lifetime: None,
            })
        })
        .collect::<Result<_>>()?;
    for o in &args.cameras {
        let (name, spec) = o.split_once('=').context("--camera expects NAME=WxH@FPS[:KBPS]")?;
        let req = reqs
            .iter_mut()
            .find(|r| r.spec.name == name)
            .with_context(|| format!("unknown camera {name:?}; cameras: {}", CAMERAS.map(|c| c.name).join(", ")))?;
        req.spec = StreamSpec::parse(name, spec)?;
    }
    Ok(reqs)
}

fn main() -> Result<()> {
    let args = Args::parse();
    gst::init()?;

    let mosaic = if args.no_mosaic {
        None
    } else {
        let (cols, rows) = parse_pair(&args.mosaic_grid, "--mosaic-grid")?;
        let (tile_w, tile_h) = parse_pair(&args.mosaic_tile, "--mosaic-tile")?;
        Some(MosaicSpec { tile_w, tile_h, cols, rows, fps: args.tick_fps, kbps: args.mosaic_kbps })
    };
    let streams = CameraStreams::new(&args.net, mosaic, args.max_cameras, "bevy-streamer (Bevy 0.18): dynamic cameras")?;

    // Control commands from client connections -> Bevy. The handler runs on the connection's thread
    // and waits (briefly) for the engine to apply the change, so the client gets a real result.
    let (tx, rx) = mpsc::channel::<Command>();
    streams.control.set_handler(move |msg| {
        let (reply_tx, reply_rx) = mpsc::channel();
        let command = match msg["cmd"].as_str()? {
            "add_camera" => match CameraRequest::from_json(msg) {
                Ok(req) => Command::Add(req, reply_tx),
                Err(e) => return Some(json!({ "ok": false, "error": format!("{e:#}") })),
            },
            "remove_camera" => match msg["name"].as_str() {
                Some(name) => Command::Remove(name.into(), reply_tx),
                None => return Some(json!({ "ok": false, "error": "\"name\" is required" })),
            },
            _ => return None,
        };
        tx.send(command).ok()?;
        Some(reply_rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| json!({ "ok": false, "error": "engine did not respond" })))
    });

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

    let tick = Duration::from_secs_f64(1.0 / args.tick_fps as f64);
    App::new()
        .insert_resource(ClearColor(SKY))
        .insert_resource(GlobalAmbientLight { color: Color::WHITE, brightness: 600.0, ..default() })
        // deterministic simulation time: every update advances exactly one tick
        .insert_resource(TimeUpdateStrategy::ManualDuration(tick))
        .insert_resource(Cameras { streams, slots: HashMap::new(), tick_fps: args.tick_fps })
        .insert_resource(Settings { shadows: !args.no_shadows, startup: startup_requests(&args)? })
        .insert_resource(Runtime { tick: 0, window_start: Instant::now(), stopped })
        .insert_resource(DroneDirector {
            period: if args.drone_cam_every > 0.0 { args.drone_cam_every } else { f32::INFINITY },
            lifetime: args.drone_cam_lifetime,
            next: if args.drone_cam_every > 0.0 { 10.0 } else { f32::INFINITY },
            count: 0,
        })
        .insert_non_send_resource(rx)
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin { primary_window: None, exit_condition: ExitCondition::DontExit, ..default() })
                .disable::<WinitPlugin>()
                // Ctrl+C is handled by the GStreamer thread (the ctrlc crate allows only one handler)
                .disable::<TerminalCtrlCHandlerPlugin>(),
        )
        .add_plugins(ScheduleRunnerPlugin::run_loop(tick))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                (scene::drive, scene::fly),
                cameras::follow_cameras,
                cameras::apply_commands,
                cameras::drone_director,
                cameras::expire_cameras,
                schedule_cameras,
                report_and_exit,
            )
                .chain(),
        )
        .run();

    gst_thread.join().ok();
    Ok(())
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut cams: ResMut<Cameras>,
    settings: Res<Settings>,
) {
    let anchors = scene::spawn_world(&mut commands, &mut meshes, &mut materials, settings.shadows);
    for req in &settings.startup {
        if let Err(e) = cams.spawn(req, 0.0, &anchors, &mut commands, &mut images) {
            error!("camera {}: {e:#}", req.spec.name);
        }
    }
    commands.insert_resource(anchors);
}

/// Per tick: render a camera only if its stream (or the mosaic) is watched and it is due at its rate.
fn schedule_cameras(cams: Res<Cameras>, mut rt: ResMut<Runtime>, mut cameras: Query<&mut Camera>, mut commands: Commands) {
    let demand = cams.streams.demand();
    let tick = rt.tick;
    rt.tick += 1;
    for (name, slot) in &cams.slots {
        let active = demand.get(name).copied().unwrap_or(false) && tick % slot.every == 0;
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

fn report_and_exit(cams: Res<Cameras>, mut rt: ResMut<Runtime>, mut exit: MessageWriter<AppExit>) {
    if rt.stopped.load(Ordering::Relaxed) {
        exit.write(AppExit::Success);
        return;
    }
    let secs = rt.window_start.elapsed().as_secs_f64();
    if secs >= 5.0 {
        let mut names: Vec<&String> = cams.slots.keys().collect();
        names.sort();
        let report: Vec<String> = names
            .iter()
            .map(|n| {
                let s = &cams.slots[*n];
                let fps = s.pushed.swap(0, Ordering::Relaxed) as f64 / secs;
                if fps > 0.0 { format!("{n} {fps:.1}/{}", s.spec.fps) } else { format!("{n} idle") }
            })
            .collect();
        println!("bevy: rendered+streamed fps: {}", report.join(" | "));
        rt.window_start = Instant::now();
    }
}
