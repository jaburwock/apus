use wgpu::TextureFormat;

// Sampling an sRGB attachment gives the phosphor shader linear light on every surface format.
pub const SCENE_FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;

pub struct Crt {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    background_and_mix: wgpu::Buffer,
    scene: Option<Scene>,
    pub(crate) mix: f32,
}

struct Scene {
    size: (u32, u32),
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

impl Crt {
    pub fn new(device: &wgpu::Device, output_format: TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("crt-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/crt.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("crt-pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(output_format.into())],
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &[("encode_srgb", f64::from(!output_format.is_srgb()))],
                    ..Default::default()
                },
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("crt-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let background_and_mix = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("crt-background-and-mix"),
            size: size_of::<[f32; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            sampler,
            background_and_mix,
            scene: None,
            mix: 1.0,
        }
    }

    pub fn prepare(&mut self, device: &wgpu::Device, size: (u32, u32)) -> &wgpu::TextureView {
        if self.scene.as_ref().is_none_or(|scene| scene.size != size) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("crt-scene"),
                size: wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: SCENE_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("crt-scene-bind-group"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.background_and_mix.as_entire_binding(),
                    },
                ],
            });
            self.scene = Some(Scene { size, view, bind_group });
        }
        &self.scene.as_ref().unwrap().view
    }

    pub fn present(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        output: &wgpu::TextureView,
        [red, green, blue, _]: [f32; 4],
    ) {
        let scene = self.scene.as_ref().expect("prepare the CRT scene before presenting");
        queue.write_buffer(
            &self.background_and_mix,
            0,
            bytemuck::bytes_of(&[red, green, blue, self.mix]),
        );
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("crt-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: output,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &scene.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
