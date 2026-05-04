use eyre::Result;
use std::convert::TryInto;

#[cfg(target_os = "macos")]
mod metal_backend;
#[cfg(not(target_os = "macos"))]
mod opencl_backend;

#[cfg(target_os = "macos")]
use metal_backend::BackendMiner;
#[cfg(not(target_os = "macos"))]
use opencl_backend::BackendMiner;

use crate::Log;

#[derive(Debug, Clone)]
pub struct MiningSettings {
    pub local_work_size: i32,
    pub kernel_size: u32,
    pub inner_iter_size: i32,
    pub kernel_name: String,
    pub sleep: u32,
    pub gpu_indices: Vec<usize>,
}

pub struct Miner {
    backend: BackendMiner,
    settings: MiningSettings,
}

#[derive(Debug, Clone, Copy)]
pub struct Work {
    header: [u8; 160],
    target: [u8; 32],
    pub nonce_idx: u32,
}

impl Work {
    pub fn from_header(header: [u8; 160], target: [u8; 32]) -> Work {
        Work {
            header,
            target,
            nonce_idx: 0,
        }
    }

    pub fn set_big_nonce(&mut self, big_nonce: u64) {
        self.header[44..52].copy_from_slice(&big_nonce.to_le_bytes());
    }

    pub fn header(&self) -> &[u8; 160] {
        &self.header
    }
}

impl Default for Work {
    fn default() -> Self {
        Work {
            header: [0; 160],
            target: [0; 32],
            nonce_idx: 0,
        }
    }
}

impl Miner {
    pub fn setup(settings: MiningSettings) -> Result<Self> {
        let backend = BackendMiner::setup(&settings)?;
        Ok(Miner { backend, settings })
    }

    pub fn list_device_names() -> Vec<String> {
        BackendMiner::list_device_names()
    }

    pub fn has_nonces_left(&self, work: &Work) -> bool {
        let searched = (work.nonce_idx as u64).saturating_mul(self.num_nonces_per_search());
        searched <= u32::MAX as u64
    }

    pub fn num_nonces_per_search(&self) -> u64 {
        self.settings.kernel_size as u64 * self.settings.inner_iter_size as u64
    }

    pub fn find_nonce(&mut self, work: &Work, log: &Log) -> Result<Option<u64>> {
        self.backend
            .find_nonce(&self.settings, work, log)
            .map_err(|e| eyre::eyre!("{}", e))
    }

    pub fn set_intensity(&mut self, intensity: i32) {
        self.settings.kernel_size = 1 << intensity;
    }

    pub fn update_gpu_index(&mut self, gpu_index: i64) -> Result<()> {
        if self.settings.gpu_indices[0] == gpu_index as usize {
            return Ok(());
        }
        let mut settings = self.settings.clone();
        settings.gpu_indices = vec![gpu_index.try_into().unwrap()];
        *self = Miner::setup(settings)?;
        Ok(())
    }
}
