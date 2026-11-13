//! `dhdr_test_depth_probe` (task HK): raster.wgsl's `depth_probe` over test_depth.c's
//! cases (80 bytes in, 8 bytes out each), as the retired `gpu_wgpu.c`'s gs_wgpu_depth_probe (last
//! at render/rust 3882d5576):
//! - the pipeline: `depth_probe` with its automatic layout and no override constants, built once
//!   per process inside a Validation error scope (a failure returns Err and the next call tries
//!   again, as the C path's NULL pipeline does);
//! - per call: a zeroed uniform of the C `Constants` size at binding 0 (the shader reads only
//!   `batch.w`, which must be 0), the cases at binding 8, the results at binding 9, one dispatch of
//!   `ceil(count / 64)` workgroups, a copy to a MAP_READ buffer, a submit and a blocking map.
//!
//! The export (`ffi.rs`) holds the MXCSR guard and has checked the slices' lengths.
use crate::device::Gpu;
use std::sync::{Arc, Mutex};

/// sizeof(Constants) in gpu_exact.inc: `uint32_t reg[128][4], info[4], palette[256]`.
const CONSTANTS_BYTES: u64 = (128 * 4 + 4 + 256) * 4;
/// One case in (raster.wgsl's DepthCase: 8 + 4 + 8 words), one result out (two words).
const CASE_BYTES: u64 = 80;
const RESULT_BYTES: u64 = 8;
/// raster.wgsl's `@workgroup_size(64)` for depth_probe.
const WORKGROUP: u32 = 64;

static PIPELINE: Mutex<Option<wgpu::ComputePipeline>> = Mutex::new(None);

/// raster.wgsl's `depth_probe` pipeline (made on the first call that succeeds).
fn pipeline(gpu: &Gpu) -> Result<wgpu::ComputePipeline, String> {
    let mut slot = PIPELINE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(p) = slot.as_ref() {
        return Ok(p.clone());
    }
    let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raster.wgsl"),
            source: wgpu::ShaderSource::Wgsl(crate::shaders::RASTER.into()),
        });
    let p = gpu
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("depth_probe"),
            layout: None,
            module: &module,
            entry_point: Some("depth_probe"),
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &[],
                zero_initialize_workgroup_memory: false,
            },
            cache: None,
        });
    if let Some(e) = pollster::block_on(scope.pop()) {
        return Err(format!("depth_probe pipeline: {e}"));
    }
    *slot = Some(p.clone());
    Ok(p)
}

fn buffer(gpu: &Gpu, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage,
        mapped_at_creation: false,
    })
}

/// Blocking read of the first `size` bytes of a MAP_READ buffer: map_async, then poll until done.
pub(crate) fn map_read(gpu: &Gpu, buffer: &wgpu::Buffer, size: u64) -> Result<Vec<u8>, String> {
    let status = Arc::new(Mutex::new(None));
    let s = status.clone();
    buffer.map_async(wgpu::MapMode::Read, 0..size, move |r| {
        *s.lock().unwrap_or_else(|p| p.into_inner()) = Some(r)
    });
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("poll: {e}"))?;
    let result = status.lock().unwrap_or_else(|p| p.into_inner()).take();
    match result {
        Some(Ok(())) => {}
        Some(Err(e)) => return Err(format!("map failed: {e}")),
        None => return Err("map callback did not run".into()),
    }
    let bytes = buffer.get_mapped_range(0..size).to_vec(); // the view drops before unmap
    buffer.unmap();
    Ok(bytes)
}

/// Runs the probe: `cases` is `count * 80` bytes, `results` `count * 2` words.
pub fn run(gpu: &Gpu, cases: &[u8], results: &mut [u32]) -> Result<(), String> {
    if cases.len() as u64 % CASE_BYTES != 0
        || (cases.len() as u64 / CASE_BYTES) * 2 != results.len() as u64
    {
        return Err(format!(
            "{} case bytes for {} result words",
            cases.len(),
            results.len()
        ));
    }
    let count = cases.len() as u64 / CASE_BYTES;
    if count == 0 {
        return Ok(());
    }
    let (in_size, out_size) = (count * CASE_BYTES, count * RESULT_BYTES);
    let groups = count.div_ceil(WORKGROUP as u64);
    let limits = gpu.device.limits();
    if groups > limits.max_compute_workgroups_per_dimension as u64 {
        return Err(format!(
            "{count} cases need {groups} workgroups (the limit is {})",
            limits.max_compute_workgroups_per_dimension
        ));
    }
    if in_size > limits.max_storage_buffer_binding_size as u64 {
        return Err(format!(
            "{count} cases are {in_size} bytes (the storage binding limit is {})",
            limits.max_storage_buffer_binding_size
        ));
    }
    let pipe = pipeline(gpu)?;
    use wgpu::BufferUsages as U;
    let constants = buffer(
        gpu,
        "probe constants",
        CONSTANTS_BYTES,
        U::UNIFORM | U::COPY_DST,
    );
    let input = buffer(gpu, "probe cases", in_size, U::STORAGE | U::COPY_DST);
    let output = buffer(gpu, "probe results", out_size, U::STORAGE | U::COPY_SRC);
    let read = buffer(gpu, "probe readback", out_size, U::MAP_READ | U::COPY_DST);
    gpu.queue
        .write_buffer(&constants, 0, &[0u8; CONSTANTS_BYTES as usize]);
    gpu.queue.write_buffer(&input, 0, cases);
    let layout = pipe.get_bind_group_layout(0);
    let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("depth_probe"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: constants.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 8,
                resource: input.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 9,
                resource: output.as_entire_binding(),
            },
        ],
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipe);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(groups as u32, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&output, 0, &read, 0, out_size);
    gpu.queue.submit([encoder.finish()]);
    let bytes = map_read(gpu, &read, out_size)?;
    for (word, b) in results.iter_mut().zip(bytes.chunks_exact(4)) {
        *word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    }
    Ok(())
}
