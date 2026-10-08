//! Headless wgpu renderer: draws N cameras into one atlas texture (one viewport per camera)
//! and reads the atlas back to the CPU as tightly packed RGBA.
//!
//! One atlas = one render pass, one GPU->CPU copy and one buffer for GStreamer per frame,
//! no matter how many cameras there are.

use anyhow::{Context, Result, bail};
use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

use crate::scene::Instance;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CameraUniform {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
}

/// Unit cube centered at the origin, 24 vertices (flat normals), 36 indices.
fn cube() -> (Vec<Vertex>, Vec<u16>) {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        // normal, u axis, v axis
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let (mut verts, mut idx) = (Vec::new(), Vec::new());
    for (n, u, v) in faces {
        let (n, u, v) = (Vec3::from(n), Vec3::from(u), Vec3::from(v));
        let base = verts.len() as u16;
        for (su, sv) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
            let p = n * 0.5 + u * su + v * sv;
            verts.push(Vertex { pos: p.into(), normal: n.into() });
        }
        idx.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (verts, idx)
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    color: wgpu::Texture,
    depth_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    index_count: u32,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,
    cam_bufs: Vec<wgpu::Buffer>,
    cam_groups: Vec<wgpu::BindGroup>,
    pub cam_w: u32,
    pub cam_h: u32,
    pub cols: u32,
    pub rows: u32,
    pub adapter_info: String,
}

impl Renderer {
    pub fn new(cam_w: u32, cam_h: u32, cameras: usize, cols: u32) -> Result<Self> {
        let rows = (cameras as u32).div_ceil(cols);
        let (width, height) = (cam_w * cols, cam_h * rows);
        // copy_texture_to_buffer needs 256-byte aligned rows; keep rows tightly packed for GStreamer
        if (width * 4) % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT != 0 {
            bail!("atlas width {width} * 4 bytes must be a multiple of 256 (use a camera width divisible by 64)");
        }

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None, // headless: no window, no surface
            ..Default::default()
        }))
        .context("no GPU adapter found")?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let cam_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene"),
            bind_group_layouts: &[Some(&cam_layout)],
            immediate_size: 0,
        });

        let vertex_layouts = [
            Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
            }),
            Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![
                    2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4
                ],
            }),
        ];
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scene"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &vertex_layouts,
            },
            primitive: wgpu::PrimitiveState { cull_mode: Some(wgpu::Face::Back), ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });

        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let color = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (width * height * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let (verts, idx) = cube();
        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cube vertices"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cube indices"),
            contents: bytemuck::cast_slice(&idx),
            usage: wgpu::BufferUsages::INDEX,
        });

        let (mut cam_bufs, mut cam_groups) = (Vec::new(), Vec::new());
        for _ in 0..cameras {
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("camera"),
                size: size_of::<CameraUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            cam_groups.push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("camera"),
                layout: &cam_layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
            }));
            cam_bufs.push(buf);
        }

        let instance_capacity = 4096;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (instance_capacity * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Renderer {
            device,
            queue,
            pipeline,
            depth_view: depth.create_view(&Default::default()),
            color,
            readback,
            vertex_buf,
            index_buf,
            index_count: idx.len() as u32,
            instance_buf,
            instance_capacity,
            cam_bufs,
            cam_groups,
            cam_w,
            cam_h,
            cols,
            rows,
            adapter_info,
        })
    }

    pub fn atlas_size(&self) -> (u32, u32) {
        (self.cam_w * self.cols, self.cam_h * self.rows)
    }

    /// Top-left pixel of camera `i` in the atlas.
    pub fn tile_origin(&self, i: usize) -> (u32, u32) {
        ((i as u32 % self.cols) * self.cam_w, (i as u32 / self.cols) * self.cam_h)
    }

    /// Render all cameras into the atlas and read it back. `cameras[i] = (eye, view_proj)`.
    pub fn render(&mut self, cameras: &[(Vec3, Mat4)], instances: &[Instance], out: &mut [u8]) -> Result<()> {
        let n = instances.len().min(self.instance_capacity);
        self.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&instances[..n]));
        for (buf, (eye, vp)) in self.cam_bufs.iter().zip(cameras) {
            let u = CameraUniform { view_proj: vp.to_cols_array_2d(), eye: eye.extend(1.0).into() };
            self.queue.write_buffer(buf, 0, bytemuck::bytes_of(&u));
        }

        let view = self.color.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cameras"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.62, g: 0.76, b: 0.92, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
            pass.set_vertex_buffer(1, self.instance_buf.slice(..));
            pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint16);
            for (i, group) in self.cam_groups.iter().enumerate() {
                let (x, y) = self.tile_origin(i);
                pass.set_viewport(x as f32, y as f32, self.cam_w as f32, self.cam_h as f32, 0.0, 1.0);
                pass.set_bind_group(0, group, &[]);
                pass.draw_indexed(0..self.index_count, 0, 0..n as u32);
            }
        }
        let (width, height) = self.atlas_size();
        enc.copy_texture_to_buffer(
            self.color.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);

        // Simple synchronous readback: wait for the GPU, copy, unmap.
        // (A real engine would use a ring of 2-3 readback buffers so the GPU never waits for the CPU.)
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback buffer"));
        self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None })?;
        out.copy_from_slice(&slice.get_mapped_range()?);
        self.readback.unmap();
        Ok(())
    }
}
