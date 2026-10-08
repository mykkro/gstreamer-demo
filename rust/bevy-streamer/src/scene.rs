//! The world: a ring road through a small city, traffic, drones, and the ego car whose children
//! are the six cameras (Bevy's transform hierarchy moves the camera rig with the car).

use std::f32::consts::{FRAC_PI_2, TAU};

use bevy::light::CascadeShadowConfigBuilder;
use bevy::prelude::*;

pub const ROAD_RADIUS: f32 = 70.0;
const EGO_LANE: f32 = 66.0;
const ONCOMING_LANE: f32 = 74.0;
const EGO_SPEED: f32 = 13.0; // m/s

/// Drives an entity along a circular lane: angle = phase + angular_speed * t.
#[derive(Component)]
pub struct Lane {
    radius: f32,
    angular_speed: f32,
    phase: f32,
}

#[derive(Component)]
pub struct Ego;

#[derive(Component)]
pub struct Drone {
    phase: f32,
}

/// Cheap deterministic hash -> [0, 1)
fn hash(x: i32, z: i32, k: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (z as u32).wrapping_mul(0xd816_3841) ^ k.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0xffff) as f32 / 65536.0
}

fn lane_transform(radius: f32, angle: f32, angular_speed: f32) -> Transform {
    let pos = Vec3::new(radius * angle.cos(), 0.0, radius * angle.sin());
    // tangent in the direction of travel; Bevy's "forward" is -Z, which `looking_to` points along it
    let dir = Vec3::new(-angle.sin(), 0.0, angle.cos()) * angular_speed.signum();
    Transform::from_translation(pos).looking_to(dir, Vec3::Y)
}

pub fn spawn_world(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    shadows: bool,
) -> Entity {
    let mut mat = |r: f32, g: f32, b: f32| {
        materials.add(StandardMaterial { base_color: Color::srgb(r, g, b), perceptual_roughness: 0.9, ..default() })
    };

    // sun with shadows (few cascades: every camera renders its own shadow cascades)
    commands.spawn((
        DirectionalLight { illuminance: 9000.0, shadows_enabled: shadows, ..default() },
        Transform::from_xyz(0.0, 0.0, 0.0).looking_to(Vec3::new(-0.45, -0.8, -0.38), Vec3::Y),
        CascadeShadowConfigBuilder { num_cascades: 2, maximum_distance: 160.0, ..default() }.build(),
    ));

    // ground, ring road (annulus), lane markings, lamp posts
    let grass = mat(0.33, 0.48, 0.3);
    commands.spawn((Mesh3d(meshes.add(Plane3d::default().mesh().size(700.0, 700.0))), MeshMaterial3d(grass)));
    commands.spawn((
        Mesh3d(meshes.add(Annulus::new(ROAD_RADIUS - 8.5, ROAD_RADIUS + 8.5).mesh().resolution(180))),
        MeshMaterial3d(mat(0.17, 0.17, 0.19)),
        Transform::from_xyz(0.0, 0.02, 0.0).with_rotation(Quat::from_rotation_x(-FRAC_PI_2)),
    ));
    let dash = meshes.add(Cuboid::new(0.25, 0.02, 2.6));
    let white = mat(0.95, 0.95, 0.9);
    let pole = meshes.add(Cylinder::new(0.18, 7.0));
    let lamp = meshes.add(Cuboid::new(0.9, 0.3, 0.9));
    let (pole_mat, lamp_mat) = (mat(0.45, 0.45, 0.48), mat(1.0, 0.9, 0.55));
    for i in 0..90 {
        let a = i as f32 / 90.0 * TAU;
        if i % 2 == 0 {
            let mut t = lane_transform(ROAD_RADIUS, a, 1.0);
            t.translation.y = 0.04;
            commands.spawn((Mesh3d(dash.clone()), MeshMaterial3d(white.clone()), t));
        }
        if i % 6 == 0 {
            for r in [ROAD_RADIUS - 11.0, ROAD_RADIUS + 11.0] {
                let p = Vec3::new(r * a.cos(), 0.0, r * a.sin());
                commands.spawn((Mesh3d(pole.clone()), MeshMaterial3d(pole_mat.clone()), Transform::from_translation(p + Vec3::Y * 3.5)));
                commands.spawn((Mesh3d(lamp.clone()), MeshMaterial3d(lamp_mat.clone()), Transform::from_translation(p + Vec3::Y * 7.1)));
            }
        }
    }

    // city blocks outside the ring, a park with trees inside
    let cube = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    let trunk = meshes.add(Cylinder::new(0.3, 1.0));
    let crown = meshes.add(Sphere::new(2.4).mesh().ico(2).unwrap());
    let (trunk_mat, crown_mat, roof_mat) = (mat(0.4, 0.28, 0.18), mat(0.2, 0.5, 0.22), mat(0.3, 0.3, 0.33));
    let step = 26;
    for gx in -9..=9 {
        for gz in -9..=9 {
            let c = Vec3::new((gx * step) as f32, 0.0, (gz * step) as f32);
            let r = c.length();
            if (r - ROAD_RADIUS).abs() < 26.0 {
                continue;
            }
            if r < ROAD_RADIUS {
                for k in 0..3 {
                    let off = Vec3::new(hash(gx, gz, k) * 18.0 - 9.0, 0.0, hash(gx, gz, k + 7) * 18.0 - 9.0);
                    let h = 4.0 + hash(gx, gz, k + 3) * 4.0;
                    commands.spawn((
                        Mesh3d(trunk.clone()),
                        MeshMaterial3d(trunk_mat.clone()),
                        Transform::from_translation(c + off + Vec3::Y * h * 0.5).with_scale(Vec3::new(1.0, h, 1.0)),
                    ));
                    commands.spawn((
                        Mesh3d(crown.clone()),
                        MeshMaterial3d(crown_mat.clone()),
                        Transform::from_translation(c + off + Vec3::Y * (h + 1.5)),
                    ));
                }
                continue;
            }
            let h = 8.0 + hash(gx, gz, 1).powi(2) * 55.0;
            let size = Vec3::new(13.0 + hash(gx, gz, 2) * 7.0, h, 13.0 + hash(gx, gz, 3) * 7.0);
            let tint = hash(gx, gz, 4);
            let m = mat(0.55 + tint * 0.35, 0.55 + hash(gx, gz, 5) * 0.25, 0.6 + (1.0 - tint) * 0.3);
            commands.spawn((Mesh3d(cube.clone()), MeshMaterial3d(m), Transform::from_translation(c + Vec3::Y * h * 0.5).with_scale(size)));
            commands.spawn((
                Mesh3d(cube.clone()),
                MeshMaterial3d(roof_mat.clone()),
                Transform::from_translation(c + Vec3::Y * (h + 1.0)).with_scale(Vec3::new(size.x * 0.4, 2.0, size.z * 0.4)),
            ));
        }
    }

    // vehicles: body + cabin as children of one entity that follows its lane
    let body = meshes.add(Cuboid::new(2.0, 1.1, 4.5));
    let cabin = meshes.add(Cuboid::new(1.8, 0.7, 2.4));
    let glass = mat(0.15, 0.17, 0.22);
    let palette = [mat(0.15, 0.35, 0.8), mat(0.95, 0.75, 0.15), mat(0.9, 0.9, 0.92), mat(0.2, 0.6, 0.35), mat(0.5, 0.2, 0.6), mat(0.1, 0.1, 0.12)];
    let spawn_car = |commands: &mut Commands, color: Handle<StandardMaterial>, lane: Lane| -> Entity {
        commands
            .spawn((lane_transform(lane.radius, lane.phase, lane.angular_speed), Visibility::default(), lane))
            .with_children(|car| {
                car.spawn((Mesh3d(body.clone()), MeshMaterial3d(color), Transform::from_xyz(0.0, 0.75, 0.0)));
                car.spawn((Mesh3d(cabin.clone()), MeshMaterial3d(glass.clone()), Transform::from_xyz(0.0, 1.6, 0.3)));
            })
            .id()
    };
    for i in 0..8 {
        let lane = Lane { radius: ONCOMING_LANE, angular_speed: -11.0 / ONCOMING_LANE, phase: i as f32 * TAU / 8.0 };
        spawn_car(commands, palette[i % palette.len()].clone(), lane);
    }
    for i in 1..5 {
        let lane = Lane { radius: EGO_LANE, angular_speed: EGO_SPEED / EGO_LANE, phase: i as f32 * TAU / 5.0 };
        spawn_car(commands, palette[(i + 2) % palette.len()].clone(), lane);
    }
    let ego = spawn_car(commands, mat(0.85, 0.15, 0.12), Lane { radius: EGO_LANE, angular_speed: EGO_SPEED / EGO_LANE, phase: 0.0 });
    commands.entity(ego).insert(Ego);

    // drones circling over the park
    let drone = meshes.add(Cuboid::new(3.0, 3.0, 3.0));
    for i in 0..6 {
        commands.spawn((Mesh3d(drone.clone()), MeshMaterial3d(palette[i].clone()), Transform::default(), Drone { phase: i as f32 }));
    }
    ego
}

pub fn drive(time: Res<Time>, mut cars: Query<(&Lane, &mut Transform)>) {
    let t = time.elapsed_secs();
    for (lane, mut tf) in &mut cars {
        *tf = lane_transform(lane.radius, lane.phase + lane.angular_speed * t, lane.angular_speed);
    }
}

pub fn fly(time: Res<Time>, mut drones: Query<(&Drone, &mut Transform)>) {
    let t = time.elapsed_secs();
    for (d, mut tf) in &mut drones {
        let a = t * 0.4 + d.phase * TAU / 6.0;
        tf.translation = Vec3::new(28.0 * a.cos(), 14.0 + 3.0 * (t * 1.3 + d.phase).sin(), 28.0 * a.sin());
        tf.rotation = Quat::from_rotation_y(t * 2.0 + d.phase);
    }
}

/// The camera rig, in the car's local frame (Bevy: -Z forward, +X right, +Y up).
pub struct CameraDef {
    pub name: &'static str,
    pub transform: fn() -> Transform,
    /// vertical field of view (radians)
    pub vfov: f32,
}

/// 90° horizontal FOV at 16:9 -> ~58.7° vertical, so front/rear/left/right cover 360°
const SURROUND_VFOV: f32 = 1.0245;

pub const CAMERAS: [CameraDef; 6] = [
    CameraDef {
        name: "front",
        transform: || Transform::from_xyz(0.0, 1.4, -2.3).looking_to(Vec3::new(0.0, -0.05, -1.0), Vec3::Y),
        vfov: SURROUND_VFOV,
    },
    CameraDef {
        name: "rear",
        transform: || Transform::from_xyz(0.0, 1.4, 2.3).looking_to(Vec3::new(0.0, -0.05, 1.0), Vec3::Y),
        vfov: SURROUND_VFOV,
    },
    CameraDef {
        name: "left",
        transform: || Transform::from_xyz(-1.1, 1.5, 0.0).looking_to(Vec3::new(-1.0, -0.05, 0.0), Vec3::Y),
        vfov: SURROUND_VFOV,
    },
    CameraDef {
        name: "right",
        transform: || Transform::from_xyz(1.1, 1.5, 0.0).looking_to(Vec3::new(1.0, -0.05, 0.0), Vec3::Y),
        vfov: SURROUND_VFOV,
    },
    CameraDef {
        name: "chase",
        transform: || Transform::from_xyz(0.0, 5.0, 12.0).looking_at(Vec3::new(0.0, 1.5, -4.0), Vec3::Y),
        vfov: 0.9,
    },
    CameraDef {
        name: "top",
        transform: || Transform::from_xyz(0.0, 55.0, 5.0).looking_at(Vec3::new(0.0, 0.0, -18.0), Vec3::Y),
        vfov: 0.9,
    },
];
