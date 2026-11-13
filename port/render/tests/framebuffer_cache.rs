//! Vulkan regression: cache overflow with commands still unsubmitted, pool reuse,
//! and repeated attachment destruction/recreation must preserve every output byte.
#![cfg(all(feature = "native", windows, target_arch = "x86_64"))]

mod common;
use common::{Adapter, Compiler};

#[test]
fn vulkan_eviction_reset_and_view_churn() {
    // Own the device so its teardown also exercises final cache destruction.
    let gpu = dhd_render::device::Gpu::create(&common::options(Adapter::Vulkan, Compiler::Fxc))
        .expect("Vulkan device");
    let device = &gpu.device;
    let queue = &gpu.queue;
    const COUNT: u32 = 300; // Exceeds the device cache's 256 entries before any submission.
    for round in 0..3 {
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let textures: Vec<_> = (0..COUNT)
            .map(|_| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("framebuffer-cache churn"),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                })
            })
            .collect();
        let views: Vec<_> = textures
            .iter()
            .map(|t| t.create_view(&Default::default()))
            .collect();
        let output = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(COUNT) * 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        // Reusing views across submissions exercises hits after encoder recycling.
        for _ in 0..2 {
            let mut encoder = device.create_command_encoder(&Default::default());
            for (i, view) in views.iter().enumerate() {
                let bits = i as u32 + round;
                let attachments = [Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(bits & 1),
                            g: f64::from((bits >> 1) & 1),
                            b: f64::from((bits >> 2) & 1),
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })];
                drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &attachments,
                    ..Default::default()
                }));
                encoder.copy_texture_to_buffer(
                    textures[i].as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &output,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: i as u64 * 256,
                            bytes_per_row: Some(256),
                            rows_per_image: None,
                        },
                    },
                    wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
            queue.submit([encoder.finish()]);
            let (send, receive) = std::sync::mpsc::channel();
            output
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    send.send(result).unwrap()
                });
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            receive.recv().unwrap().unwrap();
            let mapped = output.slice(..).get_mapped_range();
            for i in 0..COUNT {
                let bits = i + round;
                let expected = [
                    (bits & 1) as u8 * 255,
                    ((bits >> 1) & 1) as u8 * 255,
                    ((bits >> 2) & 1) as u8 * 255,
                    255,
                ];
                assert_eq!(
                    &mapped[(i as usize * 256)..][..4],
                    &expected,
                    "round {round}, view {i}"
                );
            }
            drop(mapped);
            output.unmap();
        }
        drop(views);
        drop(textures);
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        assert!(pollster::block_on(validation.pop()).is_none());
    }
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
}
