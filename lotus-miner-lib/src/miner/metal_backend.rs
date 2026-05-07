//! Metal backend for macOS GPU mining
//!
//! Uses Metal compute shaders instead of OpenCL (deprecated on macOS)

use metal::*;
use std::fs;
use std::path::Path;

use crate::miner::{MiningSettings, Work};
use crate::Log;

#[derive(Debug, thiserror::Error)]
pub enum MinerError {
    #[error("Backend error: {0}")]
    Other(String),
}

// Output buffer indices (must match lotus_macos.metal)
const FOUND: usize = 0x80;
const NFLAG: usize = 0x7F;

pub struct BackendMiner {
    command_queue: CommandQueue,
    pipeline_state: ComputePipelineState,
    header_buffer: Buffer,
    output_buffer: Buffer,
    offset_buffer: Buffer,
    target_buffer: Buffer,
}

impl BackendMiner {
    pub fn setup(settings: &MiningSettings) -> Result<Self, MinerError> {
        // Get Metal device
        let devices = Device::all();
        let device_idx = settings.gpu_indices.get(0).copied().unwrap_or(0);
        let device = devices
            .get(device_idx)
            .cloned()
            .ok_or_else(|| MinerError::Other(format!("No Metal device at index {}", device_idx)))?;

        let device_name = device.name();
        println!("[GPU] Backend: macOS Metal");
        println!("[GPU] Device #{}: {}", device_idx, device_name);
        println!("[GPU] Kernel: kernels/lotus_macos.metal");

        // Create command queue
        let command_queue = device.new_command_queue();

        // Load and compile Metal shader
        let shader_path = Path::new("kernels/lotus_macos.metal");
        let shader_source = fs::read_to_string(shader_path)
            .map_err(|e| MinerError::Other(format!("Failed to read Metal shader: {}", e)))?;

        let library = device
            .new_library_with_source(&shader_source, &CompileOptions::new())
            .map_err(|e| MinerError::Other(format!("Failed to compile Metal shader: {}", e)))?;

        let kernel = library
            .get_function("search", None)
            .map_err(|e| MinerError::Other(format!("Kernel 'search' not found: {}", e)))?;

        // Create compute pipeline
        let pipeline_state = device
            .new_compute_pipeline_state_with_function(&kernel)
            .map_err(|e| MinerError::Other(format!("Failed to create pipeline: {}", e)))?;

        // Allocate buffers
        // header_buffer: 21 u32s (84 bytes) for partial_header
        let header_buffer = device.new_buffer(84, MTLResourceOptions::StorageModeShared);

        // output_buffer: 256 u32s (1024 bytes)
        let output_buffer = device.new_buffer(1024, MTLResourceOptions::StorageModeShared);

        // offset_buffer: 1 u32 (4 bytes)
        let offset_buffer = device.new_buffer(4, MTLResourceOptions::StorageModeShared);

        // target_buffer: 8 u32s (32 bytes) for target
        let target_buffer = device.new_buffer(32, MTLResourceOptions::StorageModeShared);

        Ok(BackendMiner {
            command_queue,
            pipeline_state,
            header_buffer,
            output_buffer,
            offset_buffer,
            target_buffer,
        })
    }

    pub fn find_nonce(
        &mut self,
        settings: &MiningSettings,
        work: &Work,
        log: &Log,
    ) -> Result<Option<u64>, MinerError> {
        use sha2::Digest;
        use std::convert::TryInto;

        let base_u64 = (work.nonce_idx as u64).saturating_mul(self.num_nonces_per_search(settings));
        if base_u64 > u32::MAX as u64 {
            log.error(
                "Error: Nonce base overflow, skipping. This could be fixed by lowering \
                       rpc_poll_interval.",
            );
            return Ok(None);
        }
        let base = base_u64 as u32;

        // Build partial header
        let mut partial_header = [0u8; 84];
        partial_header[..52].copy_from_slice(&work.header[..52]);
        partial_header[52..].copy_from_slice(&sha2::Sha256::digest(&work.header[52..]));
        let mut partial_header_ints = [0u32; 21];
        for (chunk, int) in partial_header.chunks(4).zip(partial_header_ints.iter_mut()) {
            *int = u32::from_be_bytes(chunk.try_into().unwrap());
        }

        // Write partial header to buffer
        unsafe {
            std::ptr::copy_nonoverlapping(
                partial_header_ints.as_ptr() as *const _,
                self.header_buffer.contents() as *mut _,
                84,
            );
        }

        // Write offset to buffer
        unsafe {
            std::ptr::write(self.offset_buffer.contents() as *mut u32, base);
        }

        // Write target to buffer (already in little-endian format)
        unsafe {
            std::ptr::copy_nonoverlapping(
                work.target.as_ptr() as *const _,
                self.target_buffer.contents() as *mut _,
                32,
            );
        }

        // Clear output buffer
        unsafe {
            std::ptr::write_bytes(self.output_buffer.contents() as *mut u8, 0, 1024);
        }

        // Build command buffer
        let command_buffer = self.command_queue.new_command_buffer();
        let encoder = command_buffer.new_compute_command_encoder();
        encoder.set_compute_pipeline_state(&self.pipeline_state);
        encoder.set_buffer(0, Some(&self.offset_buffer), 0);
        encoder.set_buffer(1, Some(&self.header_buffer), 0);
        encoder.set_buffer(2, Some(&self.output_buffer), 0);
        encoder.set_buffer(3, Some(&self.target_buffer), 0);

        // Dispatch compute kernel
        let kernel_size = settings.kernel_size as u64;
        let threads_per_grid = MTLSize::new(kernel_size, 1, 1);
        let threads_per_threadgroup = MTLSize::new(256, 1, 1);
        encoder.dispatch_threads(threads_per_grid, threads_per_threadgroup);
        encoder.end_encoding();
        command_buffer.commit();
        command_buffer.wait_until_completed();

        // Read output buffer
        let output_ptr = self.output_buffer.contents() as *const u32;
        let output_slice = unsafe { std::slice::from_raw_parts(output_ptr, 256) };

        if output_slice[FOUND] != 0 {
            for &nonce in &output_slice[..NFLAG + 1] {
                let nonce = nonce.swap_bytes();
                if nonce != 0 {
                    let mut header = work.header;
                    header[44..48].copy_from_slice(&nonce.to_le_bytes());
                    let result_nonce = u64::from_le_bytes(header[44..52].try_into().unwrap());
                    let hash = crate::sha256::lotus_hash(&header);
                    let mut candidate_hash = hash;
                    candidate_hash.reverse();
                    /* if hash.last() != Some(&0) {
                        log.bug(
                            "BUG: found nonce's hash has no leading zero byte. Contact the \
                                   developers.",
                        );
                    } */
                    let mut below_or_equal_target = true;
                    for (&h, &t) in hash.iter().zip(work.target.iter()).rev() {
                        if h > t {
                            below_or_equal_target = false;
                            break;
                        }
                        if h < t {
                            break;
                        }
                    }
                    if below_or_equal_target {
                        log.debug(format!(
                            "Candidate: nonce={}, hash={}",
                            result_nonce,
                            hex::encode(&candidate_hash)
                        ));
                        return Ok(Some(result_nonce));
                    }
                }
            }
        }

        Ok(None)
    }

    pub fn list_device_names() -> Vec<String> {
        let devices = Device::all();
        devices
            .iter()
            .enumerate()
            .map(|(i, dev)| format!("[{}] macOS Metal - {}", i, dev.name()))
            .collect()
    }

    fn num_nonces_per_search(&self, settings: &MiningSettings) -> u64 {
        settings.kernel_size as u64 * settings.inner_iter_size as u64
    }
}
