//! Headless offscreen PNG capture under Mesa `llvmpipe` (`--llvmcapture`).
//!
//! Runs without creating any `winit` event loop or window. Consumes the shared settled
//! [`apus::capture::PreparedCapture`] cell grid, submits one Slug/CRT GPU render pass, reads back
//! `Rgba8Unorm` pixels with 256-byte row alignment stripped, and publishes the PNG atomically via
//! [`apus::capture::write_png`].

use std::sync::mpsc;

use apus::capture::{CaptureConfig, prepare_capture, write_png};
use apus_desktop::Renderer;

fn padded_bytes_per_row(width: u32) -> u32 {
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(align) * align
}

fn strip_row_padding(padded: &[u8], width: u32, height: u32, padded_row_bytes: u32) -> Vec<u8> {
    let unpadded_row = (width * 4) as usize;
    let stride = padded_row_bytes as usize;
    let mut out = Vec::with_capacity(unpadded_row * height as usize);
    for row in 0..height as usize {
        let start = row * stride;
        out.extend_from_slice(&padded[start..start + unpadded_row]);
    }
    out
}

/// Runs headless Mesa `llvmpipe` capture end-to-end, returning `Err` with a message for `stderr` on failure.
pub fn run(config: CaptureConfig, crt: bool) -> Result<(), String> {
    // GL-only headless instance verified against Mesa llvmpipe.
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::GL;
    let instance = wgpu::Instance::new(desc);

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::None,
        force_fallback_adapter: false,
        compatible_surface: None,
        ..Default::default()
    }))
    .map_err(|e| format!("failed to find GL wgpu adapter for --llvmcapture: {e}"))?;

    let info = adapter.get_info();
    if !info.name.to_ascii_lowercase().contains("llvmpipe") {
        return Err(format!(
            "--llvmcapture requires Mesa llvmpipe adapter, got {:?} ({:?})",
            info.name, info.backend
        ));
    }

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("apus-headless-device"),
        ..Default::default()
    }))
    .map_err(|e| format!("failed to create wgpu device: {e}"))?;

    let prepared = prepare_capture(&config)?;
    let (width, height) = (prepared.width, prepared.height);

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut renderer = Renderer::new(&device, &queue, format, config.weight_boost, crt);

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("apus-headless-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    renderer.render_grid(
        &device,
        &queue,
        &view,
        (width, height),
        &prepared.grid,
        prepared.cell_metrics,
    );

    let padded_row = padded_bytes_per_row(width);
    let buffer_size = u64::from(padded_row) * u64::from(height);
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("apus-headless-readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("apus-headless-copy"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = staging.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("GPU poll failed during readback: {e}"))?;
    rx.recv()
        .map_err(|_| "readback channel dropped".to_string())?
        .map_err(|e| format!("staging buffer map failed: {e}"))?;

    let pixels = {
        let mapped = slice
            .get_mapped_range()
            .map_err(|e| format!("failed to map readback buffer range: {e}"))?;
        strip_row_padding(&mapped, width, height, padded_row)
    };
    staging.unmap();

    write_png(&config.output, width, height, &pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_padding_alignment_and_stripping_preserve_exact_pixels() {
        // Width 333 * 4 = 1332 bytes -> pads to 1536 (6 * 256).
        let width = 333u32;
        let height = 5u32;
        let padded_row = padded_bytes_per_row(width);
        assert_eq!(padded_row, 1536);

        let mut padded = vec![0xEEu8; (padded_row * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let off = (y * padded_row + x * 4) as usize;
                padded[off] = (x & 0xff) as u8;
                padded[off + 1] = (y & 0xff) as u8;
                padded[off + 2] = ((x + y) & 0xff) as u8;
                padded[off + 3] = 255;
            }
        }

        let stripped = strip_row_padding(&padded, width, height, padded_row);
        assert_eq!(stripped.len(), (width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let off = ((y * width + x) * 4) as usize;
                assert_eq!(
                    &stripped[off..off + 4],
                    &[(x & 0xff) as u8, (y & 0xff) as u8, ((x + y) & 0xff) as u8, 255]
                );
            }
        }
    }
}
