//! Direct `wgpu` renderer: flat pixel-snapped rect pass + Slug outline glyph pass.
//!
//! This module has no `ratatui` dependency. It reads [`CellGrid`], resolves `u8` ANSI-256 indices
//! through [`crate::color`], emits square-corner rects for backgrounds/decorations/procedural grid
//! glyphs, and emits Slug quads for text glyphs from the embedded Fira Code curve/band textures.

use bytemuck::{Pod, Zeroable};

use crate::color;
use crate::crt::{self, Crt};
use crate::grid::{CellFlags, CellGrid};
use crate::procedural::{self, LineWeight};
use crate::slug::{self, CellMetrics, FontData, GlyphVertex, Point, TEXTURE_WIDTH, build_embedded_fonts};

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct RectVertex {
    position: [f32; 2],
    colour: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct RectUniforms {
    mvp: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct GlyphUniforms {
    mvp: [[f32; 4]; 4],
    viewport: [f32; 4],
}

fn ortho_mvp(width: f32, height: f32) -> [[f32; 4]; 4] {
    [
        [2.0 / width, 0.0, 0.0, -1.0],
        [0.0, -2.0 / height, 0.0, 1.0],
        [0.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

struct DynamicBuffer {
    buffer: wgpu::Buffer,
    capacity: u64,
    usage: wgpu::BufferUsages,
    label: &'static str,
}

impl DynamicBuffer {
    fn new(device: &wgpu::Device, label: &'static str, usage: wgpu::BufferUsages, initial: u64) -> Self {
        let capacity = initial.max(256);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            buffer,
            capacity,
            usage,
            label,
        }
    }

    fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let needed = bytes.len() as u64;
        if needed > self.capacity {
            self.capacity = needed.next_power_of_two().max(4096);
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: self.capacity,
                usage: self.usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.buffer, 0, bytes);
    }
}

/// GPU renderer owning the Slug font textures and render pipelines.
pub struct Renderer {
    font_data: FontData,
    scene_format: wgpu::TextureFormat,
    weight_boost: bool,
    crt: Option<Crt>,

    rect_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    rect_uniform_buf: wgpu::Buffer,
    glyph_uniform_buf: wgpu::Buffer,
    rect_bind_group: wgpu::BindGroup,
    glyph_bind_group: wgpu::BindGroup,

    rect_vb: DynamicBuffer,
    rect_ib: DynamicBuffer,
    glyph_vb: DynamicBuffer,
    glyph_ib: DynamicBuffer,

    // Scratch CPU buffers reused across frames to avoid per-frame allocations.
    rect_vertices: Vec<RectVertex>,
    rect_indices: Vec<u32>,
    glyph_vertices: Vec<GlyphVertex>,
    glyph_indices: Vec<u32>,
}

impl Renderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output_format: wgpu::TextureFormat,
        weight_boost: bool,
        crt_enabled: bool,
    ) -> Self {
        let crt = crt_enabled.then(|| Crt::new(device, output_format));
        let scene_format = if crt.is_some() {
            crt::SCENE_FORMAT
        } else {
            output_format
        };
        let font_data = build_embedded_fonts();

        let curve_height = font_data.curve_texture_height();
        let curve_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("slug-curve-tex"),
            size: wgpu::Extent3d {
                width: TEXTURE_WIDTH as u32,
                height: curve_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &curve_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&font_data.curve_texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((TEXTURE_WIDTH * 8) as u32),
                rows_per_image: Some(curve_height),
            },
            wgpu::Extent3d {
                width: TEXTURE_WIDTH as u32,
                height: curve_height,
                depth_or_array_layers: 1,
            },
        );
        let curve_view = curve_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let band_height = font_data.band_texture_height();
        let band_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("slug-band-tex"),
            size: wgpu::Extent3d {
                width: TEXTURE_WIDTH as u32,
                height: band_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg16Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &band_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&font_data.band_texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((TEXTURE_WIDTH * 4) as u32),
                rows_per_image: Some(band_height),
            },
            wgpu::Extent3d {
                width: TEXTURE_WIDTH as u32,
                height: band_height,
                depth_or_array_layers: 1,
            },
        );
        let band_view = band_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let rect_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect-uniforms"),
            size: size_of::<RectUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glyph_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glyph-uniforms"),
            size: size_of::<GlyphUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let rect_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rect-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let rect_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rect-bg"),
            layout: &rect_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: rect_uniform_buf.as_entire_binding(),
            }],
        });

        let glyph_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glyph-bgl"),
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
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let glyph_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph-bg"),
            layout: &glyph_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: glyph_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&curve_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&band_view),
                },
            ],
        });

        let rect_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rect-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/rect.wgsl").into()),
        });
        let glyph_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glyph-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/glyph.wgsl").into()),
        });

        let rect_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rect-pipeline-layout"),
            bind_group_layouts: &[Some(&rect_bgl)],
            immediate_size: 0,
        });
        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect-pipeline"),
            layout: Some(&rect_layout),
            vertex: wgpu::VertexState {
                module: &rect_shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<RectVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 8,
                            shader_location: 1,
                        },
                    ],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &rect_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: scene_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let glyph_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glyph-pipeline-layout"),
            bind_group_layouts: &[Some(&glyph_bgl)],
            immediate_size: 0,
        });
        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glyph-pipeline"),
            layout: Some(&glyph_layout),
            vertex: wgpu::VertexState {
                module: &glyph_shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<GlyphVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 8,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 16,
                            shader_location: 2,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 24,
                            shader_location: 3,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 40,
                            shader_location: 4,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 56,
                            shader_location: 5,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Uint32x2,
                            offset: 72,
                            shader_location: 6,
                        },
                    ],
                })],
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &[("LOG_TEXTURE_WIDTH", f64::from(TEXTURE_WIDTH.ilog2()))],
                    ..Default::default()
                },
            },
            fragment: Some(wgpu::FragmentState {
                module: &glyph_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: scene_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &[("LOG_TEXTURE_WIDTH", f64::from(TEXTURE_WIDTH.ilog2()))],
                    ..Default::default()
                },
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            font_data,
            scene_format,
            weight_boost,
            crt,
            rect_pipeline,
            glyph_pipeline,
            rect_uniform_buf,
            glyph_uniform_buf,
            rect_bind_group,
            glyph_bind_group,
            rect_vb: DynamicBuffer::new(device, "rect-vb", wgpu::BufferUsages::VERTEX, 65536),
            rect_ib: DynamicBuffer::new(device, "rect-ib", wgpu::BufferUsages::INDEX, 32768),
            glyph_vb: DynamicBuffer::new(device, "glyph-vb", wgpu::BufferUsages::VERTEX, 131072),
            glyph_ib: DynamicBuffer::new(device, "glyph-ib", wgpu::BufferUsages::INDEX, 32768),
            rect_vertices: Vec::new(),
            rect_indices: Vec::new(),
            glyph_vertices: Vec::new(),
            glyph_indices: Vec::new(),
        }
    }

    /// Computes pixel-snapped cell dimensions for a given em size in physical pixels.
    pub fn cell_metrics(&self, size_px: f32) -> CellMetrics {
        self.font_data.metrics.cell_metrics(size_px)
    }

    fn push_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, colour: [f32; 4]) {
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let base = self.rect_vertices.len() as u32;
        self.rect_vertices.extend_from_slice(&[
            RectVertex {
                position: [x0, y0],
                colour,
            },
            RectVertex {
                position: [x1, y0],
                colour,
            },
            RectVertex {
                position: [x0, y1],
                colour,
            },
            RectVertex {
                position: [x1, y1],
                colour,
            },
        ]);
        self.rect_indices
            .extend_from_slice(&[base, base + 1, base + 2, base + 2, base + 1, base + 3]);
    }

    fn push_octant(&mut self, mask: u8, x0: f32, y0: f32, cw: f32, ch: f32, colour: [f32; 4]) {
        let xs = [x0, x0 + (cw * 0.5).round(), x0 + cw];
        let ys = [
            y0,
            y0 + (ch * 0.25).round(),
            y0 + (ch * 0.5).round(),
            y0 + (ch * 0.75).round(),
            y0 + ch,
        ];

        // Row masks (2 bits per row: bit 0 = left, bit 1 = right), coalesced vertically.
        let rows = [mask & 0x03, (mask >> 2) & 0x03, (mask >> 4) & 0x03, (mask >> 6) & 0x03];
        let mut r = 0usize;
        while r < 4 {
            let rm = rows[r];
            let mut end = r + 1;
            while end < 4 && rows[end] == rm {
                end += 1;
            }
            match rm {
                0x01 => self.push_rect(xs[0], ys[r], xs[1], ys[end], colour),
                0x02 => self.push_rect(xs[1], ys[r], xs[2], ys[end], colour),
                0x03 => self.push_rect(xs[0], ys[r], xs[2], ys[end], colour),
                _ => {}
            }
            r = end;
        }
    }

    fn push_braille(&mut self, mask: u8, x0: f32, y0: f32, cw: f32, ch: f32, colour: [f32; 4]) {
        if mask == 0 {
            return;
        }
        let xs = [x0, x0 + (cw * 0.5).round(), x0 + cw];
        let ys = [
            y0,
            y0 + (ch * 0.25).round(),
            y0 + (ch * 0.5).round(),
            y0 + (ch * 0.75).round(),
            y0 + ch,
        ];
        let dw = (cw * 0.32).round().max(1.0).min((cw * 0.5 - 1.0).max(1.0));
        let dh = (ch * 0.16).round().max(1.0).min((ch * 0.25 - 1.0).max(1.0));

        const DOT_BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
        for (col, bits) in DOT_BITS.iter().enumerate() {
            let dx = ((xs[col] + xs[col + 1] - dw) * 0.5).floor();
            for (row, &bit) in bits.iter().enumerate() {
                if (mask & bit) != 0 {
                    let dy = ((ys[row] + ys[row + 1] - dh) * 0.5).floor();
                    self.push_rect(dx, dy, dx + dw, dy + dh, colour);
                }
            }
        }
    }

    fn push_box_drawing(&mut self, arms: procedural::BoxArms, x0: f32, y0: f32, cw: f32, ch: f32, colour: [f32; 4]) {
        let light = (cw / 8.0).round().max(1.0);
        let heavy = (light * 2.0).max(2.0);
        let stroke = |w: LineWeight| match w {
            LineWeight::None => 0.0,
            LineWeight::Light => light,
            LineWeight::Heavy => heavy,
        };

        let h_thick = stroke(arms.left).max(stroke(arms.right)).max(light);
        let v_thick = stroke(arms.up).max(stroke(arms.down)).max(light);
        let vx0 = x0 + ((cw - v_thick) * 0.5).floor();
        let vx1 = vx0 + v_thick;
        let hy0 = y0 + ((ch - h_thick) * 0.5).floor();
        let hy1 = hy0 + h_thick;
        let x1 = x0 + cw;
        let y1 = y0 + ch;

        // Horizontal arms
        match (arms.left, arms.right) {
            (LineWeight::None, LineWeight::None) => {}
            (l, r) if l == r => self.push_rect(x0, hy0, x1, hy1, colour),
            (LineWeight::None, _) => self.push_rect(vx0, hy0, x1, hy1, colour),
            (_, LineWeight::None) => self.push_rect(x0, hy0, vx1, hy1, colour),
            _ => {
                self.push_rect(x0, hy0, vx1, hy1, colour);
                self.push_rect(vx0, hy0, x1, hy1, colour);
            }
        }

        // Vertical arms
        match (arms.up, arms.down) {
            (LineWeight::None, LineWeight::None) => {}
            (u, d) if u == d => self.push_rect(vx0, y0, vx1, y1, colour),
            (LineWeight::None, _) => self.push_rect(vx0, hy0, vx1, y1, colour),
            (_, LineWeight::None) => self.push_rect(vx0, y0, vx1, hy1, colour),
            _ => {
                self.push_rect(vx0, y0, vx1, hy1, colour);
                self.push_rect(vx0, hy0, vx1, y1, colour);
            }
        }
    }

    fn build_geometry(&mut self, grid: &CellGrid, metrics: CellMetrics) {
        self.rect_vertices.clear();
        self.rect_indices.clear();
        self.glyph_vertices.clear();
        self.glyph_indices.clear();

        let cols = usize::from(grid.cols());
        let rows = usize::from(grid.rows());
        let cells = grid.cells();
        let cw = metrics.width_px;
        let ch = metrics.height_px;
        let is_srgb = self.scene_format.is_srgb();

        // Pass 1: cell backgrounds, coalesced into horizontal runs per row.
        for r in 0..rows {
            let y0 = r as f32 * ch;
            let y1 = y0 + ch;
            let row = &cells[r * cols..(r + 1) * cols];
            let mut c = 0usize;
            while c < cols {
                let bg_idx = row[c].bg;
                let mut end = c + 1;
                while end < cols && row[end].bg == bg_idx {
                    end += 1;
                }
                let bg = color::rgba(bg_idx, 1.0, is_srgb);
                self.push_rect(c as f32 * cw, y0, end as f32 * cw, y1, bg);
                c = end;
            }
        }

        // Pass 2: decorations, procedural grid glyphs, and Slug text quads.
        let rule_thickness = (cw / 9.0).round().max(1.0);
        for r in 0..rows {
            let y0 = r as f32 * ch;
            let y1 = y0 + ch;
            let row = &cells[r * cols..(r + 1) * cols];
            for (c, cell) in row.iter().enumerate() {
                let x0 = c as f32 * cw;
                let x1 = x0 + cw;
                let alpha = if cell.flags.contains(CellFlags::DIM) { 0.65 } else { 1.0 };
                let fg = color::rgba(cell.fg, alpha, is_srgb);

                if cell.flags.contains(CellFlags::UNDERLINED) {
                    let uy = (y0 + metrics.baseline_px + rule_thickness).min(y1 - rule_thickness);
                    self.push_rect(x0, uy, x1, uy + rule_thickness, fg);
                }
                if cell.flags.contains(CellFlags::CROSSED_OUT) {
                    let sy = (y0 + metrics.baseline_px - metrics.size_px * 0.32).round();
                    self.push_rect(x0, sy, x1, sy + rule_thickness, fg);
                }

                if cell.symbol == ' ' {
                    continue;
                }

                if let Some(mask) = procedural::octant_mask(cell.symbol) {
                    self.push_octant(mask, x0, y0, cw, ch, fg);
                } else if let Some(mask) = procedural::braille_mask(cell.symbol) {
                    self.push_braille(mask, x0, y0, cw, ch, fg);
                } else if let Some(arms) = procedural::box_drawing_arms(cell.symbol) {
                    self.push_box_drawing(arms, x0, y0, cw, ch, fg);
                } else {
                    let record = self.font_data.glyph(cell.symbol, cell.flags.contains(CellFlags::BOLD));
                    if !record.empty {
                        let origin = Point {
                            x: x0,
                            y: y0 + metrics.baseline_px,
                        };
                        let quad = slug::make_glyph_quad(record, origin, metrics.size_px, fg);
                        let base = self.glyph_vertices.len() as u32;
                        self.glyph_vertices.extend_from_slice(&quad);
                        self.glyph_indices
                            .extend_from_slice(&[base, base + 1, base + 2, base + 2, base + 1, base + 3]);
                    }
                }
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_crt_mix(&mut self, mix: f32) {
        if let Some(crt) = &mut self.crt {
            crt.mix = mix;
        }
    }

    /// Encodes and submits one full frame rendering `grid` into `view`.
    pub fn render_grid(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        target_size: (u32, u32),
        grid: &CellGrid,
        metrics: CellMetrics,
    ) {
        let (width, height) = target_size;
        self.build_geometry(grid, metrics);

        let mvp = ortho_mvp(width as f32, height as f32);
        let rect_uniforms = RectUniforms { mvp };
        let glyph_uniforms = GlyphUniforms {
            mvp,
            viewport: [
                width as f32,
                height as f32,
                if self.weight_boost { 1.0 } else { 0.0 },
                0.0,
            ],
        };
        queue.write_buffer(&self.rect_uniform_buf, 0, bytemuck::bytes_of(&rect_uniforms));
        queue.write_buffer(&self.glyph_uniform_buf, 0, bytemuck::bytes_of(&glyph_uniforms));

        self.rect_vb
            .write(device, queue, bytemuck::cast_slice(&self.rect_vertices));
        self.rect_ib
            .write(device, queue, bytemuck::cast_slice(&self.rect_indices));
        self.glyph_vb
            .write(device, queue, bytemuck::cast_slice(&self.glyph_vertices));
        self.glyph_ib
            .write(device, queue, bytemuck::cast_slice(&self.glyph_indices));

        // Clear any fractional border pixels outside the integer cell grid using the frame's
        // dominant background (typically `Surface::APP_BG()`) rather than the top-left tab bar.
        let clear_rgba = color::rgba(grid.dominant_bg(), 1.0, self.scene_format.is_srgb());

        let scene_view = self.crt.as_mut().map_or(view, |crt| crt.prepare(device, target_size));

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("apus-frame-encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("apus-frame-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: scene_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(clear_rgba[0]),
                            g: f64::from(clear_rgba[1]),
                            b: f64::from(clear_rgba[2]),
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if !self.rect_indices.is_empty() {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_bind_group(0, &self.rect_bind_group, &[]);
                pass.set_vertex_buffer(0, self.rect_vb.buffer.slice(..));
                pass.set_index_buffer(self.rect_ib.buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.rect_indices.len() as u32, 0, 0..1);
            }

            if !self.glyph_indices.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.glyph_bind_group, &[]);
                pass.set_vertex_buffer(0, self.glyph_vb.buffer.slice(..));
                pass.set_index_buffer(self.glyph_ib.buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.glyph_indices.len() as u32, 0, 0..1);
            }
        }
        if let Some(crt) = &self.crt {
            crt.present(queue, &mut encoder, view, clear_rgba);
        }
        queue.submit(std::iter::once(encoder.finish()));
    }
}
