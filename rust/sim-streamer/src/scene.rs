//! Synthetic scene: a ring road through a small city, traffic, and an ego vehicle carrying cameras.
//! Everything is a box (one instanced cube mesh), deterministic in time `t`.

use std::f32::consts::{PI, TAU};

use glam::{Mat4, Quat, Vec3};

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Instance {
    pub model: [[f32; 4]; 4],
    pub color: [f32; 4],
}

fn bx(center: Vec3, size: Vec3, yaw: f32, color: [f32; 4]) -> Instance {
    let m = Mat4::from_scale_rotation_translation(size, Quat::from_rotation_y(yaw), center);
    Instance { model: m.to_cols_array_2d(), color }
}

/// Cheap deterministic hash -> [0, 1)
fn hash(x: i32, z: i32, k: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (z as u32).wrapping_mul(0xd816_3841) ^ k.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0xffff) as f32 / 65536.0
}

pub const ROAD_RADIUS: f32 = 70.0;
const EGO_LANE: f32 = 66.0; // inner lane, angle increasing (clockwise seen from above)
const ONCOMING_LANE: f32 = 74.0; // outer lane, opposite direction
const EGO_SPEED: f32 = 13.0; // m/s

pub struct Scene {
    statics: Vec<Instance>,
}

/// Position and heading of a vehicle on a circular lane at angle `a` (radians).
fn on_lane(radius: f32, a: f32, clockwise: bool) -> (Vec3, Vec3) {
    let pos = Vec3::new(radius * a.cos(), 0.0, radius * a.sin());
    let dir = Vec3::new(-a.sin(), 0.0, a.cos()) * if clockwise { -1.0 } else { 1.0 };
    (pos, dir)
}

fn yaw_of(dir: Vec3) -> f32 {
    dir.x.atan2(dir.z)
}

fn car(pos: Vec3, dir: Vec3, color: [f32; 4]) -> [Instance; 2] {
    let yaw = yaw_of(dir);
    [
        bx(pos + Vec3::Y * 0.75, Vec3::new(2.0, 1.1, 4.5), yaw, color),
        // cabin
        bx(pos + Vec3::Y * 1.6 - dir * 0.3, Vec3::new(1.8, 0.7, 2.4), yaw, [0.15, 0.17, 0.22, 1.0]),
    ]
}

impl Scene {
    pub fn new() -> Self {
        let mut s = Vec::new();
        // ground (alpha 0 -> checker in the shader)
        s.push(bx(Vec3::new(0.0, -0.05, 0.0), Vec3::new(700.0, 0.1, 700.0), 0.0, [0.0; 4]));

        // ring road out of short straight segments, with a dashed center line and lamp posts
        let segments = 90;
        for i in 0..segments {
            let a = i as f32 / segments as f32 * TAU;
            let seg_len = TAU * ROAD_RADIUS / segments as f32 + 0.6;
            let (p, d) = on_lane(ROAD_RADIUS, a, false);
            s.push(bx(p + Vec3::Y * 0.01, Vec3::new(17.0, 0.02, seg_len * 1.1), yaw_of(d), [0.2, 0.2, 0.22, 1.0]));
            if i % 2 == 0 {
                s.push(bx(p + Vec3::Y * 0.03, Vec3::new(0.25, 0.02, seg_len * 0.6), yaw_of(d), [0.95, 0.95, 0.9, 1.0]));
            }
            if i % 6 == 0 {
                for r in [ROAD_RADIUS - 11.0, ROAD_RADIUS + 11.0] {
                    let (lp, _) = on_lane(r, a, false);
                    s.push(bx(lp + Vec3::Y * 3.5, Vec3::new(0.35, 7.0, 0.35), 0.0, [0.45, 0.45, 0.48, 1.0]));
                    s.push(bx(lp + Vec3::Y * 7.1, Vec3::new(0.9, 0.3, 0.9), 0.0, [1.0, 0.9, 0.55, 1.0]));
                }
            }
        }

        // city blocks outside the ring, a park with "trees" inside
        let step = 26;
        for gx in -9..=9 {
            for gz in -9..=9 {
                let c = Vec3::new((gx * step) as f32, 0.0, (gz * step) as f32);
                let r = c.length();
                if (r - ROAD_RADIUS).abs() < 26.0 {
                    continue; // keep the road clear
                }
                if r < ROAD_RADIUS {
                    // park: trees (trunk + crown)
                    for k in 0..3 {
                        let off = Vec3::new(hash(gx, gz, k) * 18.0 - 9.0, 0.0, hash(gx, gz, k + 7) * 18.0 - 9.0);
                        let h = 4.0 + hash(gx, gz, k + 3) * 4.0;
                        s.push(bx(c + off + Vec3::Y * h * 0.5, Vec3::new(0.6, h, 0.6), 0.0, [0.4, 0.28, 0.18, 1.0]));
                        s.push(bx(c + off + Vec3::Y * (h + 1.5), Vec3::splat(4.0), 0.0, [0.2, 0.5, 0.22, 1.0]));
                    }
                    continue;
                }
                let h = 8.0 + hash(gx, gz, 1).powi(2) * 55.0;
                let w = 13.0 + hash(gx, gz, 2) * 7.0;
                let d = 13.0 + hash(gx, gz, 3) * 7.0;
                let tint = hash(gx, gz, 4);
                let color = [0.55 + tint * 0.35, 0.55 + hash(gx, gz, 5) * 0.25, 0.6 + (1.0 - tint) * 0.3, 1.0];
                s.push(bx(c + Vec3::Y * h * 0.5, Vec3::new(w, h, d), 0.0, color));
                // roof detail
                s.push(bx(c + Vec3::Y * (h + 1.0), Vec3::new(w * 0.4, 2.0, d * 0.4), 0.0, [0.3, 0.3, 0.33, 1.0]));
            }
        }
        Scene { statics: s }
    }

    /// Ego vehicle pose at time t: (position on the road, heading)
    pub fn ego(&self, t: f32) -> (Vec3, Vec3) {
        on_lane(EGO_LANE, t * EGO_SPEED / EGO_LANE, false)
    }

    /// All instances for time t (static + moving objects).
    pub fn instances(&self, t: f32) -> Vec<Instance> {
        let mut v = self.statics.clone();
        let (ego_pos, ego_dir) = self.ego(t);
        v.extend(car(ego_pos, ego_dir, [0.85, 0.15, 0.12, 1.0]));

        let palette = [
            [0.15, 0.35, 0.8, 1.0],
            [0.95, 0.75, 0.15, 1.0],
            [0.9, 0.9, 0.92, 1.0],
            [0.2, 0.6, 0.35, 1.0],
            [0.5, 0.2, 0.6, 1.0],
            [0.1, 0.1, 0.12, 1.0],
        ];
        // oncoming traffic on the outer lane
        for i in 0..8 {
            let a = -t * 11.0 / ONCOMING_LANE + i as f32 * TAU / 8.0;
            let (p, d) = on_lane(ONCOMING_LANE, a, true);
            v.extend(car(p, d, palette[i % palette.len()]));
        }
        // same-direction traffic, same speed as the ego vehicle (constant gaps)
        for i in 1..5 {
            let a = t * EGO_SPEED / EGO_LANE + i as f32 * TAU / 5.0;
            let (p, d) = on_lane(EGO_LANE, a, false);
            v.extend(car(p, d, palette[(i + 2) % palette.len()]));
        }
        // spinning drones over the park
        for i in 0..6 {
            let a = t * 0.4 + i as f32 * TAU / 6.0;
            let c = Vec3::new(28.0 * a.cos(), 14.0 + 3.0 * (t * 1.3 + i as f32).sin(), 28.0 * a.sin());
            v.push(bx(c, Vec3::splat(3.0), t * 2.0 + i as f32, palette[i % palette.len()]));
        }
        v
    }
}

/// A camera mounted on (or following) the ego vehicle.
pub struct CameraDef {
    pub name: &'static str,
    /// mount position relative to the vehicle: (forward, up, right) in meters
    pub mount: Vec3,
    /// yaw relative to the vehicle heading (radians, positive = to the left), and pitch (radians, positive = up)
    pub yaw: f32,
    pub pitch: f32,
    /// vertical field of view (radians)
    pub vfov: f32,
}

/// 90° horizontal FOV at 16:9 -> ~58.7° vertical
const SURROUND_VFOV: f32 = 1.0245;

pub const CAMERAS: [CameraDef; 6] = [
    CameraDef { name: "front", mount: Vec3::new(2.3, 1.4, 0.0), yaw: 0.0, pitch: -0.05, vfov: SURROUND_VFOV },
    CameraDef { name: "rear", mount: Vec3::new(-2.3, 1.4, 0.0), yaw: PI, pitch: -0.05, vfov: SURROUND_VFOV },
    CameraDef { name: "left", mount: Vec3::new(0.0, 1.5, -1.1), yaw: PI / 2.0, pitch: -0.05, vfov: SURROUND_VFOV },
    CameraDef { name: "right", mount: Vec3::new(0.0, 1.5, 1.1), yaw: -PI / 2.0, pitch: -0.05, vfov: SURROUND_VFOV },
    CameraDef { name: "chase", mount: Vec3::new(-12.0, 5.0, 0.0), yaw: 0.0, pitch: -0.25, vfov: 0.9 },
    CameraDef { name: "top", mount: Vec3::new(-5.0, 55.0, 0.0), yaw: 0.0, pitch: -1.2, vfov: 0.9 },
];

impl CameraDef {
    /// (eye position, view-projection matrix) for this camera at the given vehicle pose.
    pub fn view_proj(&self, pos: Vec3, dir: Vec3, aspect: f32) -> (Vec3, Mat4) {
        let right = dir.cross(Vec3::Y).normalize();
        let eye = pos + dir * self.mount.x + Vec3::Y * self.mount.y + right * self.mount.z;
        let look = Quat::from_rotation_y(self.yaw) * dir;
        let look = (look * self.pitch.cos() + Vec3::Y * self.pitch.sin()).normalize();
        let view = glam::camera::rh::view::look_to_mat4(eye, look, Vec3::Y);
        // wgpu uses D3D-style clip space (y up, depth 0..1)
        let proj = glam::camera::rh::proj::directx::perspective(self.vfov, aspect, 0.1, 900.0);
        (eye, proj * view)
    }
}
