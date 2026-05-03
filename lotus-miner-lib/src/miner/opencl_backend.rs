//! OpenCL backend for Linux/Windows GPU mining

use ocl::{
    builders::{DeviceSpecifier, ProgramBuilder},
    Buffer, Context, Device, Kernel, Platform, Queue,
};
use std::convert::TryInto;

use crate::miner::{MiningSettings, Work};
use crate::Log;

#[derive(Debug, thiserror::Error)]
pub enum MinerError {
    #[error("Ocl error: {0:?}")]
    Ocl(ocl::Error),
}

impl From<ocl::Error> for MinerError {
    fn from(err: ocl::Error) -> Self {
        MinerError::Ocl(err)
    }
}

pub struct BackendMiner {
    search_kernel: Kernel,
    header_buffer: Buffer<u32>,
    buffer: Buffer<u32>,
}

impl BackendMiner {
    pub fn setup(settings: &MiningSettings) -> Result<Self, MinerError> {
        let kernel_file = format!("kernels/{}.cl", settings.kernel_name);
        let mut prog_builder = ProgramBuilder::new();
        prog_builder
            .src_file(&kernel_file)
            .cmplr_def("WORKSIZE", settings.local_work_size)
            .cmplr_def("ITERATIONS", settings.inner_iter_size);
        let platforms = Platform::list();
        println!("[OpenCL] Platform: OpenCL");
        println!("[OpenCL] Kernel: {}", kernel_file);
        for (platform_idx, platform) in platforms.iter().enumerate() {
            println!("[OpenCL] Platform {}: {}", platform_idx, platform.name().unwrap_or("<invalid platform>".to_string()));
            let devices = Device::list_all(platform).map_err(MinerError::Ocl)?;
            for (device_idx, device) in devices.iter().enumerate() {
                println!("- device {}: {}", device_idx, device.name().map_err(MinerError::Ocl)?);
            }
        }
        let mut platform_device = None;
        let mut gpu_index = 0;
        for cur_platform in platforms {
            if let Ok(devices) = Device::list_all(cur_platform.clone()) {
                for cur_device in devices {
                    if gpu_index == settings.gpu_indices[0] {
                        platform_device = Some((cur_platform, cur_device));
                    }
                    gpu_index += 1;
                }
            }
        }
        let (platform, device) = platform_device.expect("No such GPU");
        let ctx = Context::builder()
            .platform(platform.clone())
            .devices(DeviceSpecifier::Single(device.clone()))
            .build()
            .map_err(MinerError::Ocl)?;
        let queue = Queue::new(&ctx, device, None).map_err(MinerError::Ocl)?;
        prog_builder.devices(DeviceSpecifier::Single(device.clone()));
        let program = prog_builder.build(&ctx).map_err(MinerError::Ocl)?;
        let mut kernel_builder = Kernel::builder();
        kernel_builder
            .program(&program)
            .name("search")
            .queue(queue.clone());
        let buffer = Buffer::builder()
            .len(0xff)
            .queue(queue.clone())
            .build()
            .map_err(MinerError::Ocl)?;
        let header_buffer = Buffer::builder()
            .len(0xff)
            .queue(queue)
            .build()
            .map_err(MinerError::Ocl)?;
        let search_kernel = kernel_builder
            .arg_named("offset", 0u32)
            .arg_named("partial_header", None::<&Buffer<u32>>)
            .arg_named("output", None::<&Buffer<u32>>)
            .build()
            .map_err(MinerError::Ocl)?;
        Ok(BackendMiner {
            search_kernel,
            buffer,
            header_buffer,
        })
    }

    pub fn find_nonce(
        &mut self,
        settings: &MiningSettings,
        work: &Work,
        log: &Log,
    ) -> Result<Option<u64>, MinerError> {
        use sha2::Digest;

        let base_u64 = (work.nonce_idx as u64).saturating_mul(self.num_nonces_per_search(settings));
        if base_u64 > u32::MAX as u64 {
            log.error(
                "Error: Nonce base overflow, skipping. This could be fixed by lowering \
                           rpc_poll_interval.",
            );
            return Ok(None);
        }
        let base = base_u64 as u32;
        let mut partial_header = [0u8; 84];
        partial_header[..52].copy_from_slice(&work.header[..52]);
        partial_header[52..].copy_from_slice(&sha2::Sha256::digest(&work.header[52..]));
        let mut partial_header_ints = [0u32; 21];
        for (chunk, int) in partial_header.chunks(4).zip(partial_header_ints.iter_mut()) {
            *int = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        self.header_buffer
            .write(&partial_header_ints[..])
            .enq()
            .map_err(MinerError::Ocl)?;
        self.search_kernel
            .set_arg("partial_header", &self.header_buffer)
            .map_err(MinerError::Ocl)?;
        self.search_kernel
            .set_arg("output", &self.buffer)
            .map_err(MinerError::Ocl)?;
        self.search_kernel.set_arg("offset", base).map_err(MinerError::Ocl)?;
        let mut vec = vec![0; self.buffer.len()];
        self.buffer.write(&vec).enq().map_err(MinerError::Ocl)?;
        let cmd = self
            .search_kernel
            .cmd()
            .global_work_size(settings.kernel_size);
        unsafe {
            cmd.enq().map_err(MinerError::Ocl)?;
        }
        self.buffer.read(&mut vec).enq().map_err(MinerError::Ocl)?;
        if vec[0x80] != 0 {
            let mut header = work.header;
            for &nonce in &vec[..0x7f] {
                let nonce = nonce.swap_bytes();
                if nonce != 0 {
                    header[44..48].copy_from_slice(&nonce.to_le_bytes());
                    let result_nonce = u64::from_le_bytes(header[44..52].try_into().unwrap());
                    let hash = crate::sha256::lotus_hash(&header);
                    let mut candidate_hash = hash;
                    candidate_hash.reverse();
                    log.info(format!(
                        "Candidate: nonce={}, hash={}",
                        result_nonce,
                        hex::encode(&candidate_hash)
                    ));
                    if hash.last() != Some(&0) {
                        log.bug(
                            "BUG: found nonce's hash has no leading zero byte. Contact the \
                                   developers.",
                        );
                    }
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
                        return Ok(Some(result_nonce));
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn list_device_names() -> Vec<String> {
        let platforms = Platform::list();
        let mut device_names = Vec::new();
        for platform in platforms.iter() {
            let platform_name = platform.name().unwrap_or("<invalid platform>".to_string());
            let devices = Device::list_all(platform).unwrap_or(vec![]);
            for device in devices.iter() {
                device_names.push(format!(
                    "{} - {}",
                    platform_name,
                    device.name().unwrap_or("<invalid device>".to_string())
                ));
            }
        }
        device_names
    }

    fn num_nonces_per_search(&self, settings: &MiningSettings) -> u64 {
        settings.kernel_size as u64 * settings.inner_iter_size as u64
    }
}
