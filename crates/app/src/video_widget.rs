//! Renders decoded I420 frames on the GPU: three single-channel textures per
//! stream, converted to RGB in a fragment shader.

use iced::mouse;
use iced::wgpu;
use iced::widget::shader::{self, Viewport};
use iced::{Element, Length, Rectangle};
use parking_lot::Mutex;
use screenshare_media::video::I420Frame;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Latest decoded frame of a stream, written by the decoder thread.
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<Arc<I420Frame>>>,
    generation: AtomicU64,
}

impl FrameSlot {
    pub fn publish(&self, frame: Arc<I420Frame>) {
        *self.frame.lock() = Some(frame);
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn latest(&self) -> (u64, Option<Arc<I420Frame>>) {
        let generation = self.generation.load(Ordering::Acquire);
        (generation, self.frame.lock().clone())
    }

    pub fn has_frame(&self) -> bool {
        self.frame.lock().is_some()
    }
}

pub fn video<'a, Message: 'a>(key: u64, slot: Arc<FrameSlot>) -> Element<'a, Message> {
    shader::Shader::new(VideoProgram { key, slot })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

struct VideoProgram {
    key: u64,
    slot: Arc<FrameSlot>,
}

impl<Message> shader::Program<Message> for VideoProgram {
    type State = ();
    type Primitive = VideoPrimitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, _bounds: Rectangle) -> VideoPrimitive {
        let (generation, frame) = self.slot.latest();
        VideoPrimitive {
            key: self.key,
            generation,
            frame,
        }
    }
}

#[derive(Debug)]
pub struct VideoPrimitive {
    key: u64,
    generation: u64,
    frame: Option<Arc<I420Frame>>,
}

struct Planes {
    size: (u32, u32),
    textures: [wgpu::Texture; 3],
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    generation: u64,
    last_used: u64,
}

pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    linear_out: bool,
    streams: HashMap<u64, Planes>,
    frame: u64,
}

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video yuv"),
            source: wgpu::ShaderSource::Wgsl(include_str!("video_yuv.wgsl").into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video yuv"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(2),
                texture_entry(3),
                texture_entry(4),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video yuv"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video yuv"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video yuv"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
            linear_out: format.is_srgb(),
            streams: HashMap::new(),
            frame: 0,
        }
    }

    fn trim(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        self.streams
            .retain(|_, p| frame.saturating_sub(p.last_used) < 300);
    }
}

impl VideoPipeline {
    fn create_planes(&self, device: &wgpu::Device, width: u32, height: u32) -> Planes {
        let plane = |w: u32, h: u32| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("video plane"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let textures = [plane(width, height), plane(cw, ch), plane(cw, ch)];
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let views: Vec<_> = textures
            .iter()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect();
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video yuv"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
            ],
        });
        Planes {
            size: (width, height),
            textures,
            uniforms,
            bind_group,
            generation: u64::MAX,
            last_used: 0,
        }
    }
}

fn upload(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], width: u32, height: u32) {
    if data.len() < (width * height) as usize {
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

impl shader::Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        pipeline: &mut VideoPipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        let Some(frame) = &self.frame else { return };
        if frame.width == 0 || frame.height == 0 {
            return;
        }
        let size = (frame.width, frame.height);
        if pipeline
            .streams
            .get(&self.key)
            .is_none_or(|p| p.size != size)
        {
            let planes = pipeline.create_planes(device, frame.width, frame.height);
            pipeline.streams.insert(self.key, planes);
        }
        let linear_out = pipeline.linear_out;
        let now = pipeline.frame;
        let planes = pipeline.streams.get_mut(&self.key).expect("inserted above");
        planes.last_used = now;
        if planes.generation != self.generation {
            planes.generation = self.generation;
            let (cw, ch) = (frame.chroma_width(), frame.chroma_height());
            upload(
                queue,
                &planes.textures[0],
                &frame.y,
                frame.width,
                frame.height,
            );
            upload(queue, &planes.textures[1], &frame.u, cw, ch);
            upload(queue, &planes.textures[2], &frame.v, cw, ch);
        }

        let frame_aspect = frame.width as f32 / frame.height as f32;
        let bounds_aspect = bounds.width.max(1.0) / bounds.height.max(1.0);
        let scale = if frame_aspect > bounds_aspect {
            [1.0, bounds_aspect / frame_aspect]
        } else {
            [frame_aspect / bounds_aspect, 1.0]
        };
        let uniforms = [scale[0], scale[1], if linear_out { 1.0 } else { 0.0 }, 0.0];
        queue.write_buffer(&planes.uniforms, 0, bytemuck::cast_slice(&uniforms));
    }

    fn draw(&self, pipeline: &VideoPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.frame.is_none() {
            return true;
        }
        if let Some(planes) = pipeline.streams.get(&self.key) {
            render_pass.set_pipeline(&pipeline.pipeline);
            render_pass.set_bind_group(0, &planes.bind_group, &[]);
            render_pass.draw(0..6, 0..1);
        }
        true
    }
}
