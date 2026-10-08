//! Camera lifecycle: requests (from the control server, the startup rig, or the drone director),
//! spawning a camera with its own render-target image + readback + GStreamer stream, and despawning.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use anyhow::{Context, Result, bail, ensure};
use bevy::camera::RenderTarget;
use bevy::pbr::{DistanceFog, FogFalloff};
use bevy::prelude::*;
use bevy::render::gpu_readback::ReadbackComplete;
use bevy::render::render_resource::{TextureFormat, TextureUsages};
use gst_demo::Channel;
use gst_demo::streams::{CameraStreams, StreamSpec};
use serde_json::{Value, json};

use crate::scene::Anchors;

pub const SKY: Color = Color::srgb(0.62, 0.76, 0.92);

/// What a camera is attached to.
#[derive(Debug, Clone, Copy)]
pub enum Anchor {
    World,
    Ego,
    Car(usize),
    Drone(usize),
}

impl Anchor {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s.split_once(':') {
            None if s == "world" => Anchor::World,
            None if s == "ego" => Anchor::Ego,
            Some(("car", n)) => Anchor::Car(n.parse()?),
            Some(("drone", n)) => Anchor::Drone(n.parse()?),
            _ => bail!("expected world | ego | car:N | drone:N, got {s:?}"),
        })
    }

    fn entity(self, anchors: &Anchors) -> Result<Option<Entity>> {
        Ok(match self {
            Anchor::World => None,
            Anchor::Ego => Some(anchors.ego),
            Anchor::Car(i) => Some(*anchors.cars.get(i).with_context(|| format!("no car:{i} (0..{})", anchors.cars.len()))?),
            Anchor::Drone(i) => {
                Some(*anchors.drones.get(i).with_context(|| format!("no drone:{i} (0..{})", anchors.drones.len()))?)
            }
        })
    }
}

#[derive(Debug, Clone)]
pub enum Look {
    /// the anchor's forward direction (-Z)
    Forward,
    /// a point: world coordinates for `World`, relative to the anchor otherwise
    Point(Vec3),
    /// keep looking at another anchor (tracking camera)
    At(Anchor),
}

#[derive(Debug, Clone)]
pub enum Placement {
    /// fixed transform in the ego car's frame (the startup rig)
    Rig(Transform),
    Anchored { anchor: Anchor, position: Vec3, look: Look },
}

#[derive(Debug, Clone)]
pub struct CameraRequest {
    pub spec: StreamSpec,
    pub placement: Placement,
    /// vertical field of view (radians)
    pub vfov: f32,
    /// remove automatically after this many seconds of simulation time
    pub lifetime: Option<f32>,
}

impl CameraRequest {
    /// Parse an `add_camera` control message:
    /// `{"cmd":"add_camera","name":"cctv1","width":640,"height":360,"fps":15,"kbps":800,
    ///   "attach":"world|ego|car:N|drone:N","position":[x,y,z],"look_at":[x,y,z]|"forward"|"ego"|"car:N"|"drone:N",
    ///   "fov":60,"lifetime":20}` (everything but "name" is optional)
    pub fn from_json(msg: &Value) -> Result<Self> {
        let name = msg["name"].as_str().context("\"name\" is required")?;
        let int = |k: &str, d: u64| msg.get(k).map_or(Some(d), Value::as_u64).with_context(|| format!("{k:?} must be a number"));
        let spec = StreamSpec::new(
            name,
            int("width", 640)? as u32,
            int("height", 360)? as u32,
            int("fps", 15)? as u32,
            msg.get("kbps").and_then(Value::as_u64).map(|k| k as u32),
        );
        let anchor = Anchor::parse(msg["attach"].as_str().unwrap_or("world"))?;
        let vec3 = |v: &Value| -> Option<Vec3> {
            let a = v.as_array()?;
            Some(Vec3::new(a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32, a.get(2)?.as_f64()? as f32))
        };
        let position = match msg.get("position") {
            Some(v) => vec3(v).context("\"position\" must be [x, y, z]")?,
            None => match anchor {
                Anchor::World => Vec3::new(0.0, 40.0, 110.0),
                Anchor::Ego | Anchor::Car(_) => Vec3::new(0.0, 2.5, 0.0),
                Anchor::Drone(_) => Vec3::new(0.0, -2.0, 0.0),
            },
        };
        let look = match msg.get("look_at") {
            None => match anchor {
                Anchor::Ego | Anchor::Car(_) => Look::Forward,
                Anchor::World | Anchor::Drone(_) => Look::At(Anchor::Ego),
            },
            Some(Value::String(s)) if s == "forward" => Look::Forward,
            Some(Value::String(s)) => Look::At(Anchor::parse(s)?),
            Some(v) => Look::Point(vec3(v).context("\"look_at\" must be [x, y, z] or an anchor")?),
        };
        let fov = msg.get("fov").and_then(Value::as_f64).unwrap_or(60.0) as f32;
        ensure!((5.0..=150.0).contains(&fov), "\"fov\" must be 5..150 degrees");
        Ok(CameraRequest {
            spec,
            placement: Placement::Anchored { anchor, position, look },
            vfov: fov.to_radians(),
            lifetime: msg.get("lifetime").and_then(Value::as_f64).map(|l| l as f32),
        })
    }
}

/// Commands from other threads (control server) to the Bevy world; the reply goes back on `reply`.
pub enum Command {
    Add(CameraRequest, mpsc::Sender<Value>),
    Remove(String, mpsc::Sender<Value>),
}

/// A camera that is not a child of its anchor: follows a position and/or tracks a target every frame.
#[derive(Component)]
pub struct Follow {
    anchor: Option<Entity>,
    offset: Vec3,
    look: FollowLook,
}

enum FollowLook {
    Direction(Vec3),
    Point(Vec3),
    Entity(Entity),
}

/// Runtime bookkeeping per camera.
pub struct CamSlot {
    pub spec: StreamSpec,
    pub image: Handle<Image>,
    pub camera: Entity,
    pub readback: Entity,
    /// render every n-th tick (tick rate / camera rate)
    pub every: u64,
    pub pushed: Arc<AtomicU32>,
    pub expires: Option<f32>,
}

#[derive(Resource)]
pub struct Cameras {
    pub streams: CameraStreams,
    pub slots: HashMap<String, CamSlot>,
    pub tick_fps: u32,
}

impl Cameras {
    /// Spawn a camera: GStreamer branch first (may fail: name taken, limits…), then the Bevy side.
    pub fn spawn(
        &mut self,
        req: &CameraRequest,
        now: f32,
        anchors: &Anchors,
        commands: &mut Commands,
        images: &mut Assets<Image>,
    ) -> Result<Channel> {
        let spec = &req.spec;
        ensure!(
            spec.fps > 0 && self.tick_fps % spec.fps == 0,
            "fps {} must divide the engine tick rate {} (e.g. {})",
            spec.fps,
            self.tick_fps,
            (1..=self.tick_fps).filter(|f| self.tick_fps % f == 0).map(|f| f.to_string()).collect::<Vec<_>>().join(", ")
        );
        // resolve anchors before touching GStreamer, so a bad request has no side effects
        let placement = match &req.placement {
            Placement::Rig(local) => (Some(anchors.ego), *local, None),
            Placement::Anchored { anchor, position, look } => {
                let parent = anchor.entity(anchors)?;
                let child_of_car = matches!(anchor, Anchor::Ego | Anchor::Car(_));
                match look {
                    // rigid mount on a car: a child entity, transform propagation does the rest
                    Look::Forward if child_of_car => (parent, Transform::from_translation(*position).looking_to(Vec3::NEG_Z, Vec3::Y), None),
                    Look::Point(p) if child_of_car => (parent, Transform::from_translation(*position).looking_at(*p, Vec3::Y), None),
                    // world / drone / tracking cameras: top-level, updated by `follow_cameras`
                    _ => {
                        let look = match look {
                            Look::Forward => FollowLook::Direction(Vec3::new(0.0, -0.3, -1.0)),
                            Look::Point(p) => FollowLook::Point(*p),
                            Look::At(a) => FollowLook::Entity(a.entity(anchors)?.context("cannot look at \"world\"")?),
                        };
                        (None, Transform::from_translation(*position), Some(Follow { anchor: parent, offset: *position, look }))
                    }
                }
            }
        };

        let (channel, appsrc) = self.streams.add_camera(spec)?;

        // Each camera renders into its own image. sRGB format: the bytes we read back are display-ready.
        let mut image = Image::new_target_texture(spec.width, spec.height, TextureFormat::Rgba8UnormSrgb, None);
        image.texture_descriptor.usage |= TextureUsages::COPY_SRC; // needed for the readback copy
        let image = images.add(image);

        let (parent, transform, follow) = placement;
        let mut cam = commands.spawn((
            Camera3d::default(),
            // starts inactive: schedule_cameras turns it on when someone is watching
            Camera { is_active: false, ..default() },
            RenderTarget::Image(image.clone().into()),
            Projection::Perspective(PerspectiveProjection {
                fov: req.vfov,
                aspect_ratio: spec.width as f32 / spec.height as f32,
                near: 0.1,
                far: 900.0,
                ..default()
            }),
            DistanceFog { color: SKY, falloff: FogFalloff::Linear { start: 80.0, end: 380.0 }, ..default() },
            transform,
            Name::new(spec.name.clone()),
        ));
        if let Some(f) = follow {
            cam.insert(f);
        }
        let camera = cam.id();
        if let Some(parent) = parent {
            commands.entity(parent).add_child(camera); // rigid mount: moves with the car
        }

        // Readback entity: the Readback component is attached only on ticks where the camera renders
        // (see schedule_cameras); the observer forwards the bytes to this camera's appsrc.
        let pushed = Arc::new(AtomicU32::new(0));
        let counter = pushed.clone();
        let (w, h) = (spec.width as usize, spec.height as usize);
        let readback = commands
            .spawn(Name::new(format!("readback {}", spec.name)))
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

        self.slots.insert(
            spec.name.clone(),
            CamSlot {
                spec: spec.clone(),
                image,
                camera,
                readback,
                every: (self.tick_fps / spec.fps) as u64,
                pushed,
                expires: req.lifetime.map(|l| now + l),
            },
        );
        Ok(channel)
    }

    pub fn despawn(&mut self, name: &str, commands: &mut Commands, images: &mut Assets<Image>) -> Result<()> {
        let slot = self.slots.remove(name).with_context(|| format!("no camera {name:?}"))?;
        commands.entity(slot.camera).despawn();
        commands.entity(slot.readback).despawn(); // also removes its observer
        images.remove(&slot.image);
        self.streams.remove_camera(name)
    }
}

/// Apply queued add/remove commands from the control server (one reply per command).
pub fn apply_commands(
    rx: NonSend<mpsc::Receiver<Command>>,
    mut cams: ResMut<Cameras>,
    anchors: Res<Anchors>,
    time: Res<Time>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
) {
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            Command::Add(req, reply) => {
                let r = cams.spawn(&req, time.elapsed_secs(), &anchors, &mut commands, &mut images);
                let _ = reply.send(match r {
                    Ok(ch) => json!({ "ok": true, "channel": ch }),
                    Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
                });
            }
            Command::Remove(name, reply) => {
                let r = cams.despawn(&name, &mut commands, &mut images);
                let _ = reply.send(match r {
                    Ok(()) => json!({ "ok": true }),
                    Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
                });
            }
        }
    }
}

/// Remove cameras whose lifetime is over.
pub fn expire_cameras(mut cams: ResMut<Cameras>, time: Res<Time>, mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let now = time.elapsed_secs();
    let expired: Vec<String> = cams.slots.iter().filter(|(_, s)| s.expires.is_some_and(|e| now >= e)).map(|(n, _)| n.clone()).collect();
    for name in expired {
        if let Err(e) = cams.despawn(&name, &mut commands, &mut images) {
            warn!("expire {name}: {e:#}");
        }
    }
}

/// Move world/drone/tracking cameras (after the vehicles and drones moved).
pub fn follow_cameras(mut cams: Query<(&Follow, &mut Transform)>, targets: Query<&Transform, Without<Follow>>) {
    for (f, mut tf) in &mut cams {
        let base = f.anchor.and_then(|a| targets.get(a).ok()).map_or(Vec3::ZERO, |t| t.translation);
        let eye = base + f.offset;
        let target = match f.look {
            FollowLook::Direction(d) => eye + d,
            FollowLook::Point(p) => base + p,
            FollowLook::Entity(e) => targets.get(e).map_or(eye + Vec3::NEG_Z, |t| t.translation + Vec3::Y),
        };
        *tf = Transform::from_translation(eye).looking_at(target, Vec3::Y);
    }
}

/// Scripted dynamic cameras: every `period` seconds a drone gets a camera tracking the ego car,
/// which lives for `lifetime` seconds. Shows channels appearing/disappearing without any client.
#[derive(Resource)]
pub struct DroneDirector {
    pub period: f32,
    pub lifetime: f32,
    pub next: f32,
    pub count: u32,
}

pub fn drone_director(
    mut director: ResMut<DroneDirector>,
    mut cams: ResMut<Cameras>,
    anchors: Res<Anchors>,
    time: Res<Time>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
) {
    let now = time.elapsed_secs();
    if now < director.next {
        return;
    }
    director.next = now + director.period;
    director.count += 1;
    let n = director.count;
    let req = CameraRequest {
        spec: StreamSpec::new(&format!("dronecam_{n}"), 640, 360, 15, Some(800)),
        placement: Placement::Anchored {
            anchor: Anchor::Drone(n as usize % anchors.drones.len()),
            position: Vec3::new(0.0, -2.0, 0.0),
            look: Look::At(Anchor::Ego),
        },
        vfov: 40f32.to_radians(),
        lifetime: Some(director.lifetime),
    };
    if let Err(e) = cams.spawn(&req, now, &anchors, &mut commands, &mut images) {
        warn!("drone director: {e:#}");
    }
}
