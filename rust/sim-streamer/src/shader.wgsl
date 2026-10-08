// One instanced cube mesh draws the whole scene (ground, roads, buildings, cars...).
// Per instance: model matrix + color. color.a == 0 marks the ground (procedural checker pattern).

struct Camera {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
};
@group(0) @binding(0) var<uniform> cam: Camera;

struct VertexIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) color: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec4<f32>,
    @location(2) world: vec3<f32>,
};

@vertex
fn vs_main(v: VertexIn) -> VertexOut {
    let model = mat4x4<f32>(v.m0, v.m1, v.m2, v.m3);
    let world = model * vec4<f32>(v.pos, 1.0);
    var out: VertexOut;
    out.clip = cam.view_proj * world;
    // boxes have axis-aligned local normals, so the model matrix + normalize is exact enough
    out.normal = normalize((model * vec4<f32>(v.normal, 0.0)).xyz);
    out.color = v.color;
    out.world = world.xyz;
    return out;
}

const SKY: vec3<f32> = vec3<f32>(0.62, 0.76, 0.92);
const SUN: vec3<f32> = vec3<f32>(0.45, 0.80, 0.38);

@fragment
fn fs_main(f: VertexOut) -> @location(0) vec4<f32> {
    var base = f.color.rgb;
    if (f.color.a < 0.5) {
        // ground: 5 m checker, slightly varied green
        let c = floor(f.world.x / 5.0) + floor(f.world.z / 5.0);
        base = select(vec3<f32>(0.33, 0.47, 0.30), vec3<f32>(0.38, 0.53, 0.33), c % 2.0 == 0.0);
    }
    let n = normalize(f.normal);
    let diffuse = max(dot(n, normalize(SUN)), 0.0);
    var lit = base * (0.35 + 0.75 * diffuse);
    // distance fog for depth cues
    let dist = distance(f.world, cam.eye.xyz);
    let fog = clamp((dist - 60.0) / 260.0, 0.0, 1.0);
    lit = mix(lit, SKY, fog);
    return vec4<f32>(lit, 1.0);
}
