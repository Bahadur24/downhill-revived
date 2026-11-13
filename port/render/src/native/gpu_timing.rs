//! Opt-in GPU picture timestamps; asynchronous, with no wait or slot reuse while mapped.
//! DHD_GPU_TIMING requests timestamp features at device creation. Does nothing otherwise.
use crate::device::Gpu;
use crate::exact::readback::{map_start, MapStatus};

struct Slot {
    query: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    status: Option<MapStatus>,
    pending: bool,
}

#[derive(Default)]
pub struct Timing {
    slots: Vec<Slot>,
    active: Option<usize>,
    samples: u32,
    nanoseconds: f64,
}

impl Timing {
    pub fn begin(&mut self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder) {
        if !gpu
            .device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
        {
            return;
        }
        // submit already polls. Consume only maps whose callback has run.
        for slot in &mut self.slots {
            let result = slot.status.as_ref().and_then(|s| {
                s.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .map(Result::is_ok)
            });
            if let Some(result) = result {
                if result {
                    {
                        let bytes = slot.read.slice(..16).get_mapped_range();
                        let first = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                        let last = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
                        self.nanoseconds += last.wrapping_sub(first) as f64
                            * gpu.queue.get_timestamp_period() as f64;
                        self.samples += 1;
                    }
                    slot.read.unmap();
                }
                slot.status = None;
            }
        }
        if self.samples >= 240 {
            crate::host::log(&format!(
                "gpu_time: pictures={} mean_ms={:.3}",
                self.samples,
                self.nanoseconds / self.samples as f64 / 1e6
            ));
            self.samples = 0;
            self.nanoseconds = 0.0;
        }
        if self.slots.len() < 3 && self.slots.iter().all(|s| s.status.is_some() || s.pending) {
            let buffer = |label, usage| {
                gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: 256,
                    usage,
                    mapped_at_creation: false,
                })
            };
            self.slots.push(Slot {
                query: gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("picture timestamps"),
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                }),
                resolve: buffer(
                    "picture timestamp resolve",
                    wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                ),
                read: buffer(
                    "picture timestamp map",
                    wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                ),
                status: None,
                pending: false,
            });
        }
        self.active = self
            .slots
            .iter()
            .position(|s| s.status.is_none() && !s.pending);
        if let Some(index) = self.active {
            encoder.write_timestamp(&self.slots[index].query, 0);
        }
    }

    pub fn end(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if let Some(index) = self.active.take() {
            let slot = &mut self.slots[index];
            encoder.write_timestamp(&slot.query, 1);
            encoder.resolve_query_set(&slot.query, 0..2, &slot.resolve, 0);
            encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.read, 0, 16);
            slot.pending = true;
        }
    }

    pub fn map(&mut self) {
        for slot in &mut self.slots {
            if slot.pending {
                slot.status = Some(map_start(&slot.read, 0..16));
                slot.pending = false;
            }
        }
    }
}
