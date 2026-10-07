//! GPU video surface for screen shares.
//!
//! A custom wgpu primitive keeps textures per stream and uploads straight
//! from the decoder's buffer:
//! - H.264 streams arrive as NV12: luma and chroma planes go into two
//!   textures and the fragment shader does the BT.709 → RGB conversion.
//! - Tile streams arrive as RGBA: only changed tiles are uploaded.
//!
//! The shader also letterboxes, rounds the corners and draws the remote
//! cursor for tile streams (H.264 streams have it composited in).

use std::collections::HashMap;
use std::sync::Arc;

use iced::widget::shader::{self, Viewport};
use iced::{Rectangle, mouse, wgpu};

use crate::screen::codec::Rect;
use crate::screen::{PixelFormat, VideoSink};

pub struct Video {
    pub sink: Arc<VideoSink>,
    pub radius: f32,
}

impl<Message> shader::Program<Message> for Video {
    type State = ();
    type Primitive = Primitive;

    fn draw(&self, _: &(), _: mouse::Cursor, _: Rectangle) -> Primitive {
        Primitive { sink: self.sink.clone(), radius: self.radius }
    }
}

#[derive(Debug)]
pub struct Primitive {
    sink: Arc<VideoSink>,
    radius: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    /// Video quad in NDC of the widget viewport: x0, y0 (top), x1, y1 (bottom).
    rect: [f32; 4],
    /// Quad size in physical px, corner radius in physical px.
    size: [f32; 4],
    /// Cursor x/y in texture px, visible flag, physical px per texture px.
    cursor: [f32; 4],
    /// Texture width/height, scale factor, unused.
    tex: [f32; 4],
    /// Mode (0 = RGBA, 1 = NV12), surface is sRGB (1/0), unused ×2.
    mode: [f32; 4],
}

struct Entry {
    plane0: wgpu::Texture,
    plane1: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    format: PixelFormat,
    size: (u32, u32),
    epoch: u64,
    last_used: u64,
}

pub struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    srgb_surface: bool,
    entries: HashMap<u64, Entry>,
    frame: u64,
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("discostu video"),
            source: wgpu::ShaderSource::Wgsl(include_str!("video.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("discostu video layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(3),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("discostu video pipeline layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("discostu video pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("discostu video sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { pipeline, layout, sampler, srgb_surface: format.is_srgb(), entries: HashMap::new(), frame: 0 }
    }

    fn trim(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        self.entries.retain(|_, e| frame - e.last_used < 600);
    }
}

fn make_texture(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("discostu video plane"),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

impl Pipeline {
    fn create_entry(&self, device: &wgpu::Device, format: PixelFormat, w: u32, h: u32, epoch: u64) -> Entry {
        let (plane0, plane1) = match format {
            PixelFormat::Rgba => {
                // Screen pixels are sRGB-encoded: match the surface so sampling
                // and presenting round-trip exactly.
                let f = if self.srgb_surface {
                    wgpu::TextureFormat::Rgba8UnormSrgb
                } else {
                    wgpu::TextureFormat::Rgba8Unorm
                };
                (make_texture(device, w, h, f), make_texture(device, 1, 1, wgpu::TextureFormat::Rg8Unorm))
            }
            PixelFormat::Nv12 => (
                make_texture(device, w, h, wgpu::TextureFormat::R8Unorm),
                make_texture(device, w / 2, h / 2, wgpu::TextureFormat::Rg8Unorm),
            ),
        };
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("discostu video uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let v0 = plane0.create_view(&wgpu::TextureViewDescriptor::default());
        let v1 = plane1.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("discostu video bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&v0) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&v1) },
            ],
        });
        Entry { plane0, plane1, bind_group, uniforms, format, size: (w, h), epoch, last_used: self.frame }
    }
}

/// Uploads a rectangle of a plane whose rows are `stride` bytes apart.
fn upload(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], stride: usize, bpp: usize, r: Rect) {
    let offset = r.y * stride + r.x * bpp;
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x: r.x as u32, y: r.y as u32, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        &data[offset..],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride as u32),
            rows_per_image: Some(r.h as u32),
        },
        wgpu::Extent3d { width: r.w as u32, height: r.h as u32, depth_or_array_layers: 1 },
    );
}

/// Merges horizontally adjacent tiles in the same row into single uploads.
fn coalesce(dirty: &mut [Rect]) -> Vec<Rect> {
    dirty.sort_by_key(|r| (r.y, r.x));
    let mut out: Vec<Rect> = Vec::with_capacity(dirty.len());
    for r in dirty.iter() {
        match out.last_mut() {
            Some(last) if last.y == r.y && last.h == r.h && last.x + last.w == r.x => last.w += r.w,
            _ => out.push(*r),
        }
    }
    out
}

impl shader::Primitive for Primitive {
    type Pipeline = Pipeline;

    fn prepare(
        &self,
        pipeline: &mut Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let mut f = self.sink.frame.lock().expect("sink lock");
        if f.width == 0 || f.height == 0 {
            return;
        }
        let (w, h) = (f.width as u32, f.height as u32);
        let stale = pipeline
            .entries
            .get(&self.sink.id)
            .is_none_or(|e| e.size != (w, h) || e.epoch != f.epoch || e.format != f.format);
        if stale {
            let entry = pipeline.create_entry(device, f.format, w, h, f.epoch);
            pipeline.entries.insert(self.sink.id, entry);
            f.full_dirty = true;
        }
        let frame_no = pipeline.frame;
        let srgb = pipeline.srgb_surface;
        let entry = pipeline.entries.get_mut(&self.sink.id).expect("entry just ensured");
        entry.last_used = frame_no;

        let (fw, fh) = (f.width, f.height);
        match f.format {
            PixelFormat::Nv12 => {
                if f.full_dirty {
                    let (y, uv) = f.data.split_at(fw * fh);
                    upload(queue, &entry.plane0, y, fw, 1, Rect { x: 0, y: 0, w: fw, h: fh });
                    upload(queue, &entry.plane1, uv, fw, 2, Rect { x: 0, y: 0, w: fw / 2, h: fh / 2 });
                }
            }
            PixelFormat::Rgba => {
                if f.full_dirty {
                    upload(queue, &entry.plane0, &f.data, fw * 4, 4, Rect { x: 0, y: 0, w: fw, h: fh });
                } else if !f.dirty.is_empty() {
                    let mut dirty = std::mem::take(&mut f.dirty);
                    for r in coalesce(&mut dirty) {
                        upload(queue, &entry.plane0, &f.data, fw * 4, 4, r);
                    }
                    dirty.clear();
                    f.dirty = dirty; // keep the allocation
                }
            }
        }
        f.full_dirty = false;
        f.dirty.clear();
        let cursor = f.cursor;
        let mode = if f.format == PixelFormat::Nv12 { 1.0 } else { 0.0 };
        drop(f);

        // Letterbox into the widget bounds (logical px).
        let scale = (bounds.width / w as f32).min(bounds.height / h as f32);
        let (dw, dh) = (w as f32 * scale, h as f32 * scale);
        let x0 = (bounds.width - dw) / 2.0;
        let y0 = (bounds.height - dh) / 2.0;
        let ndc_x = |x: f32| x / bounds.width * 2.0 - 1.0;
        let ndc_y = |y: f32| 1.0 - y / bounds.height * 2.0;
        let sf = viewport.scale_factor() as f32;
        let uniforms = Uniforms {
            rect: [ndc_x(x0), ndc_y(y0), ndc_x(x0 + dw), ndc_y(y0 + dh)],
            size: [dw * sf, dh * sf, self.radius * sf, 0.0],
            cursor: [cursor.x as f32, cursor.y as f32, cursor.visible as u8 as f32, dw * sf / w as f32],
            tex: [w as f32, h as f32, sf, 0.0],
            mode: [mode, srgb as u8 as f32, 0.0, 0.0],
        };
        queue.write_buffer(&entry.uniforms, 0, bytemuck::bytes_of(&uniforms));
    }

    fn draw(&self, pipeline: &Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if let Some(entry) = pipeline.entries.get(&self.sink.id) {
            render_pass.set_pipeline(&pipeline.pipeline);
            render_pass.set_bind_group(0, &entry.bind_group, &[]);
            render_pass.draw(0..6, 0..1);
        }
        true
    }
}
