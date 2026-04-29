mod block;
mod miner;
pub mod settings;
mod sha256;

use eyre::Result;
pub use miner::Miner;
use primitive_types::U256;
pub use settings::ConfigSettings;

use std::{
    convert::TryInto,
    fmt::Display,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

use block::{create_block, Block, GetRawUnsolvedBlockResponse};
use miner::{MiningSettings, Work};
use rand::{Rng, SeedableRng};
use reqwest::{RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{Mutex, MutexGuard},
};

pub struct Server {
    client: reqwest::Client,
    miner: std::sync::Mutex<Miner>,
    node_settings: Mutex<NodeSettings>,
    stratum_settings: Mutex<StratumSettings>,
    block_state: Mutex<BlockState>,
    rng: Mutex<rand::rngs::StdRng>,
    metrics_timestamp: Mutex<SystemTime>,
    metrics_nonces: AtomicU64,
    log: Log,
    report_hashrate_interval: Duration,
}

pub struct NodeSettings {
    pub bitcoind_url: String,
    pub bitcoind_user: String,
    pub bitcoind_password: String,
    pub rpc_poll_interval: u64,
    pub miner_addr: String,
}

pub struct StratumSettings {
    pub stratum_url: Option<String>,
    pub stratum_worker_name: Option<String>,
    pub stratum_password: String,
    pub stratum_extranonce1: String,
    pub stratum_extranonce2_size: usize,
    pub stratum_difficulty: f64,
}

pub struct Log {
    logs: std::sync::RwLock<Vec<LogEntry>>,
    hashrates: std::sync::RwLock<Vec<HashrateEntry>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSeverity {
    Info,
    Warn,
    Error,
    Bug,
}

pub struct LogEntry {
    pub msg: String,
    pub severity: LogSeverity,
    pub timestamp: chrono::DateTime<chrono::Local>,
}

pub struct HashrateEntry {
    pub hashrate: f64,
    pub timestamp: chrono::DateTime<chrono::Local>,
}

struct BlockState {
    current_work: Work,
    current_block: Option<Block>,
    next_block: Option<Block>,
    extra_nonce: u64,
    stratum_job: Option<StratumJob>,
}

#[derive(Debug, Clone)]
struct StratumJob {
    job_id: String,
    prevhash: String,
    coinbase1: String,
    coinbase2: String,
    merkle_branches: Vec<String>,
    version: String,
    nbits: String,
    ntime_hex_6b: String,
    extranonce2: String,
}

pub type ServerRef = Arc<Server>;

impl Server {
    pub fn from_config(config: ConfigSettings, report_hashrate_interval: Duration) -> Self {
        let mining_settings = MiningSettings {
            local_work_size: 256,
            inner_iter_size: 16,
            kernel_size: 1 << config.kernel_size,
            kernel_name: "lotus_og".to_string(),
            sleep: 0,
            gpu_indices: vec![config.gpu_index as usize],
        };
        let miner = Miner::setup(mining_settings.clone()).unwrap();

        let stratum_url = config
            .stratum_url
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let stratum_worker_name = config
            .stratum_worker_name
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Server {
            miner: std::sync::Mutex::new(miner),
            client: reqwest::Client::new(),
            node_settings: Mutex::new(NodeSettings {
                bitcoind_url: config.rpc_url.clone(),
                bitcoind_user: config.rpc_user.clone(),
                bitcoind_password: config.rpc_password.clone(),
                rpc_poll_interval: config.rpc_poll_interval.try_into().unwrap(),
                miner_addr: config.mine_to_address.clone(),
            }),
            stratum_settings: Mutex::new(StratumSettings {
                stratum_url,
                stratum_worker_name,
                stratum_password: config.stratum_password.unwrap_or_else(|| "x".to_string()),
                stratum_extranonce1: "00000000".to_string(),
                stratum_extranonce2_size: 4,
                stratum_difficulty: 1.0,
            }),
            block_state: Mutex::new(BlockState {
                current_work: Work::default(),
                current_block: None,
                next_block: None,
                extra_nonce: 0,
                stratum_job: None,
            }),
            rng: Mutex::new(rand::rngs::StdRng::from_entropy()),
            metrics_timestamp: Mutex::new(SystemTime::now()),
            metrics_nonces: AtomicU64::new(0),
            log: Log::new(),
            report_hashrate_interval,
        }
    }

    pub async fn run(self: ServerRef) -> Result<(), Box<dyn std::error::Error>> {
        let stratum_enabled = self
            .stratum_settings
            .lock()
            .await
            .stratum_url
            .as_ref()
            .is_some();
        if stratum_enabled {
            self.log().info("Starting in STRATUM mode");
            run_stratum(self).await
        } else {
            self.log().info("Starting in SOLO JSON-RPC mode");
            run_solo(self).await
        }
    }

    pub async fn node_settings<'a>(&'a self) -> MutexGuard<'a, NodeSettings> {
        self.node_settings.lock().await
    }

    pub async fn stratum_settings<'a>(&'a self) -> MutexGuard<'a, StratumSettings> {
        self.stratum_settings.lock().await
    }

    pub fn miner<'a>(&'a self) -> std::sync::MutexGuard<'a, Miner> {
        self.miner.lock().unwrap()
    }

    pub fn log(&self) -> &Log {
        &self.log
    }

    async fn stratum_worker_full_name(&self) -> Result<String> {
        let miner_addr = self.node_settings.lock().await.miner_addr.clone();
        if miner_addr.trim().is_empty() {
            return Err(eyre::eyre!("mine_to_address must be set for stratum mode"));
        }
        let worker_name = self
            .stratum_settings
            .lock()
            .await
            .stratum_worker_name
            .clone();
        let full = match worker_name {
            Some(worker) if !worker.is_empty() => format!("{}.{}", miner_addr, worker),
            _ => miner_addr,
        };
        Ok(full)
    }
}

async fn run_solo(server: ServerRef) -> Result<(), Box<dyn std::error::Error>> {
    let t1 = tokio::spawn({
        let server = Arc::clone(&server);
        async move {
            let log = server.log();
            loop {
                if let Err(err) = update_next_block(&server).await {
                    log.error(format!("update_next_block error: {:?}", err));
                }
                let rpc_poll_interval = server.node_settings.lock().await.rpc_poll_interval;
                tokio::time::sleep(Duration::from_secs(rpc_poll_interval)).await;
            }
        }
    });
    let t2 = tokio::spawn({
        let server = Arc::clone(&server);
        async move {
            let log = server.log();
            loop {
                if let Err(err) = mine_some_nonces(Arc::clone(&server)).await {
                    log.error(format!("mine_some_nonces error: {:?}", err));
                }
            }
        }
    });
    t1.await?;
    t2.await?;
    Ok(())
}

async fn run_stratum(server: ServerRef) -> Result<(), Box<dyn std::error::Error>> {
    let mut backoff_secs: u64 = 1;
    loop {
        let stratum_url = server
            .stratum_settings
            .lock()
            .await
            .stratum_url
            .clone()
            .ok_or_else(|| eyre::eyre!("stratum_url is not set"))?;
        server
            .log()
            .info(format!("Connecting to stratum {}", stratum_url));

        match run_stratum_session(Arc::clone(&server), &stratum_url).await {
            Ok(()) => {
                server.log().warn("Stratum session ended, reconnecting");
            }
            Err(err) => {
                server
                    .log()
                    .error(format!("Stratum session error: {}", err));
            }
        }

        server
            .log()
            .warn(format!("Retrying stratum in {}s", backoff_secs));
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

async fn run_stratum_session(server: ServerRef, stratum_url: &str) -> Result<()> {
    let stream = TcpStream::connect(stratum_url).await?;
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let mut line = String::new();
    let mut req_id: u64 = 1;

    let worker_name = server.stratum_worker_full_name().await?;
    let stratum_password = server
        .stratum_settings
        .lock()
        .await
        .stratum_password
        .clone();

    let subscribe_id = req_id;
    req_id += 1;
    let subscribe = serde_json::json!({
        "id": subscribe_id,
        "method": "mining.subscribe",
        "params": []
    });
    writer_half
        .write_all(format!("{}\n", subscribe).as_bytes())
        .await?;
    server.log().info("Sent mining.subscribe");

    let authorize_id = req_id;
    req_id += 1;
    let authorize = serde_json::json!({
        "id": authorize_id,
        "method": "mining.authorize",
        "params": [worker_name, stratum_password]
    });
    writer_half
        .write_all(format!("{}\n", authorize).as_bytes())
        .await?;
    server.log().info("Sent mining.authorize");
    server.log().info("Optional methods (extranonce.subscribe / suggest_difficulty / set_extranonce) are scaffolded but disabled by default");

    loop {
        line.clear();
        tokio::select! {
            read = reader.read_line(&mut line) => {
                let n = read?;
                if n == 0 {
                    return Err(eyre::eyre!("stratum socket closed"));
                }
                handle_stratum_line(&server, line.trim_end(), subscribe_id, authorize_id).await?;
            }
            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                if let Some((job_id, extranonce2, ntime, nonce_hex)) = mine_some_nonces_stratum(Arc::clone(&server)).await? {
                    let submit_id = req_id;
                    req_id += 1;
                    let submit = serde_json::json!({
                        "id": submit_id,
                        "method": "mining.submit",
                        "params": [
                            server.stratum_worker_full_name().await?,
                            job_id,
                            extranonce2,
                            ntime,
                            nonce_hex,
                        ]
                    });
                    writer_half
                        .write_all(format!("{}\n", submit).as_bytes())
                        .await?;
                    server.log().info(format!("Sent mining.submit id={}", submit_id));
                }
            }
        }
    }
}

async fn handle_stratum_line(
    server: &Server,
    line: &str,
    subscribe_id: u64,
    authorize_id: u64,
) -> Result<()> {
    let v: Value = serde_json::from_str(line)?;

    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        match method {
            "mining.set_difficulty" => {
                let diff = v
                    .get("params")
                    .and_then(|p| p.as_array())
                    .and_then(|a| a.first())
                    .and_then(|d| d.as_f64())
                    .unwrap_or(1.0)
                    .max(0.0000001);
                server.stratum_settings.lock().await.stratum_difficulty = diff;
                server.log().info(format!("set_difficulty={}", diff));
            }
            "mining.notify" => {
                let params = v
                    .get("params")
                    .and_then(|p| p.as_array())
                    .ok_or_else(|| eyre::eyre!("invalid mining.notify params"))?;
                let job_id = params
                    .get(0)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let prevhash =
                    normalize_hex_len(params.get(1).and_then(|v| v.as_str()).unwrap_or(""), 64);
                let coinbase1 = params
                    .get(2)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let coinbase2 = params
                    .get(3)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let merkle_branches = params
                    .get(4)
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let version =
                    normalize_hex_len(params.get(5).and_then(|v| v.as_str()).unwrap_or(""), 8);
                let nbits =
                    normalize_hex_len(params.get(6).and_then(|v| v.as_str()).unwrap_or(""), 8);
                let ntime_hex_6b = normalize_hex_len(
                    params
                        .get(7)
                        .and_then(|v| v.as_str())
                        .unwrap_or("000000000000"),
                    12,
                );
                let clean_jobs = params.get(8).and_then(|v| v.as_bool()).unwrap_or(false);

                let mut header = [0u8; 160];
                if let Some(prevhash) = params.get(1).and_then(|v| v.as_str()) {
                    let prev = hex::decode(prevhash).unwrap_or_default();
                    let copy_len = prev.len().min(32);
                    header[..copy_len].copy_from_slice(&prev[..copy_len]);
                }
                if let Some(version) = params.get(5).and_then(|v| v.as_str()) {
                    let version = hex::decode(version).unwrap_or_default();
                    let copy_len = version.len().min(4);
                    header[32..32 + copy_len].copy_from_slice(&version[..copy_len]);
                }
                if let Some(nbits) = params.get(6).and_then(|v| v.as_str()) {
                    let nbits = hex::decode(nbits).unwrap_or_default();
                    let copy_len = nbits.len().min(4);
                    header[36..36 + copy_len].copy_from_slice(&nbits[..copy_len]);
                }
                if let Ok(ntime) = hex::decode(&ntime_hex_6b) {
                    let copy_len = ntime.len().min(6);
                    header[40..40 + copy_len].copy_from_slice(&ntime[..copy_len]);
                }

                let difficulty = server.stratum_settings.lock().await.stratum_difficulty;
                let target = difficulty_to_target(difficulty);
                let mut block_state = server.block_state.lock().await;
                block_state.current_work = Work::from_header(header, target);
                if clean_jobs {
                    block_state.current_work.nonce_idx = 0;
                }
                let extranonce2_size = server
                    .stratum_settings
                    .lock()
                    .await
                    .stratum_extranonce2_size;
                let extranonce2 = random_extranonce2(extranonce2_size);
                block_state.stratum_job = Some(StratumJob {
                    job_id: job_id.clone(),
                    prevhash,
                    coinbase1,
                    coinbase2,
                    merkle_branches,
                    version,
                    nbits,
                    ntime_hex_6b,
                    extranonce2,
                });
                drop(block_state);
                server.log().info(format!(
                    "Work update: mining.notify job_id={} clean_jobs={} difficulty={}",
                    job_id, clean_jobs, difficulty
                ));
            }
            _ => {}
        }
        return Ok(());
    }

    let id = v.get("id").and_then(|id| id.as_u64()).unwrap_or(0);
    let err = v.get("error");
    if id == subscribe_id {
        if err.is_some() && !err.unwrap().is_null() {
            server
                .log()
                .error(format!("mining.subscribe failed: {}", err.unwrap()));
        } else {
            let extranonce1 = v
                .get("result")
                .and_then(|r| r.as_array())
                .and_then(|a| a.get(1))
                .and_then(|s| s.as_str())
                .unwrap_or("00000000")
                .to_string();
            let extranonce2_size = v
                .get("result")
                .and_then(|r| r.as_array())
                .and_then(|a| a.get(2))
                .and_then(|n| n.as_u64())
                .unwrap_or(4) as usize;
            let mut settings = server.stratum_settings.lock().await;
            settings.stratum_extranonce1 = extranonce1;
            settings.stratum_extranonce2_size = extranonce2_size;
            server.log().info(format!(
                "mining.subscribe OK extranonce2_size={}",
                extranonce2_size
            ));
        }
    } else if id == authorize_id {
        if err.is_some() && !err.unwrap().is_null() {
            server
                .log()
                .error(format!("mining.authorize failed: {}", err.unwrap()));
        } else {
            let ok = v.get("result").and_then(|r| r.as_bool()).unwrap_or(false);
            if ok {
                server.log().info("mining.authorize OK");
            } else {
                server.log().error("mining.authorize rejected by server");
            }
        }
    } else if err.is_some() && !err.unwrap().is_null() {
        server
            .log()
            .warn(format!("Stratum response id={} error={}", id, err.unwrap()));
    } else if let Some(result_bool) = v.get("result").and_then(|r| r.as_bool()) {
        if result_bool {
            server.log().info(format!("Share accepted id={}", id));
        } else {
            server
                .log()
                .warn(format!("Share rejected id={} (result=false)", id));
        }
    }

    Ok(())
}

fn difficulty_to_target(difficulty: f64) -> [u8; 32] {
    let target = target_for_share_difficulty(difficulty).unwrap_or_else(|_| U256::MAX);
    let mut out = [0u8; 32];
    target.to_big_endian(&mut out);
    out
}

fn share_meets_server_target(
    job: &StratumJob,
    extranonce1: &str,
    difficulty: f64,
    nonce_hex_8b: &str,
) -> Result<bool> {
    let coinbase = hex::decode(format!(
        "{}{}{}{}",
        job.coinbase1, extranonce1, job.extranonce2, job.coinbase2
    ))?;
    let mut merkle = sha256d(&coinbase).to_vec();
    for branch_hex in &job.merkle_branches {
        let branch = hex::decode(branch_hex)?;
        let mut concat = Vec::with_capacity(64);
        concat.extend_from_slice(&merkle);
        concat.extend_from_slice(&branch);
        merkle = sha256d(&concat).to_vec();
    }

    let mut header = Vec::with_capacity(4 + 32 + 32 + 6 + 4 + 8);
    header.extend_from_slice(&hex::decode(&job.version)?);
    header.extend_from_slice(&hex::decode(&job.prevhash)?);
    header.extend_from_slice(&merkle);
    header.extend_from_slice(&hex::decode(&job.ntime_hex_6b)?);
    header.extend_from_slice(&hex::decode(&job.nbits)?);
    header.extend_from_slice(&hex::decode(nonce_hex_8b)?);

    let mut hash_be = sha256d(&header);
    hash_be.reverse();
    let hash_u256 = U256::from_big_endian(&hash_be);
    let share_target = target_for_share_difficulty(difficulty)?;
    Ok(hash_u256 <= share_target)
}

fn target_for_share_difficulty(difficulty: f64) -> Result<U256> {
    const DIFF1_TARGET_HEX: &str =
        "00000000ffff0000000000000000000000000000000000000000000000000000";
    const DIFF_SCALE: u128 = 100_000_000;

    if difficulty <= 0.0 || !difficulty.is_finite() {
        return Err(eyre::eyre!("invalid difficulty"));
    }
    let scaled = (difficulty * DIFF_SCALE as f64).round();
    if scaled <= 0.0 || !scaled.is_finite() {
        return Err(eyre::eyre!("invalid difficulty scale"));
    }
    let scaled_u = U256::from(scaled as u128);
    let mut diff1_bytes = [0u8; 32];
    let raw = hex::decode(DIFF1_TARGET_HEX)?;
    diff1_bytes.copy_from_slice(&raw);
    let diff1 = U256::from_big_endian(&diff1_bytes);
    let scaled_diff1 = diff1 * U256::from(DIFF_SCALE);
    let mut target = scaled_diff1 / scaled_u;
    if target.is_zero() {
        target = U256::one();
    }
    Ok(target)
}

fn sha256d(bytes: &[u8]) -> [u8; 32] {
    let h1 = sha2::Sha256::digest(bytes);
    let h2 = sha2::Sha256::digest(&h1);
    h2.into()
}

fn random_extranonce2(extranonce2_size: usize) -> String {
    let bytes = extranonce2_size.max(1).min(16);
    let mut out = String::new();
    for _ in 0..bytes {
        let b: u8 = rand::thread_rng().gen();
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn normalize_hex_len(value: &str, len: usize) -> String {
    let mut s = value.trim().to_ascii_lowercase();
    if s.len() > len {
        s.truncate(len);
    }
    while s.len() < len {
        s.push('0');
    }
    s
}

async fn mine_some_nonces_stratum(
    server: ServerRef,
) -> Result<Option<(String, String, String, String)>> {
    let log = server.log();
    let block_state = server.block_state.lock().await;
    let Some(job) = block_state.stratum_job.clone() else {
        return Ok(None);
    };

    let mut work = block_state.current_work;
    let big_nonce = server.rng.lock().await.gen();
    work.set_big_nonce(big_nonce);
    drop(block_state);

    let (nonce, num_nonces_per_search) = tokio::task::spawn_blocking({
        let server = Arc::clone(&server);
        move || {
            let mut miner = server.miner.lock().unwrap();
            if !miner.has_nonces_left(&work) {
                work.nonce_idx = 0;
            }
            miner
                .find_nonce(&work, server.log())
                .map(|nonce| (nonce, miner.num_nonces_per_search()))
        }
    })
    .await
    .unwrap()?;

    let mut block_state = server.block_state.lock().await;
    block_state.current_work.nonce_idx = block_state.current_work.nonce_idx.wrapping_add(1);
    server
        .metrics_nonces
        .fetch_add(num_nonces_per_search, Ordering::AcqRel);
    update_hashrate(server.as_ref(), log).await;

    if let Some(nonce) = nonce {
        let nonce_hex = format!("{:016x}", nonce);
        let settings = server.stratum_settings.lock().await;
        let difficulty = settings.stratum_difficulty;
        let extranonce1 = settings.stratum_extranonce1.clone();
        drop(settings);
        if !share_meets_server_target(&job, &extranonce1, difficulty, &nonce_hex)? {
            log.warn(format!(
                "Candidate filtered (below stratum difficulty): job_id={} extranonce2={} nonce={}",
                job.job_id, job.extranonce2, nonce_hex
            ));
            return Ok(None);
        }
        log.info(format!(
            "Candidate share job_id={} extranonce2={} nonce={}",
            job.job_id, job.extranonce2, nonce_hex
        ));
        return Ok(Some((
            job.job_id,
            job.extranonce2,
            job.ntime_hex_6b,
            nonce_hex,
        )));
    }

    if block_state.current_work.nonce_idx == u32::MAX {
        block_state.current_work.nonce_idx = 0;
    }

    Ok(None)
}

async fn update_hashrate(server: &Server, log: &Log) {
    let mut timestamp = server.metrics_timestamp.lock().await;
    let elapsed = match SystemTime::now().duration_since(*timestamp) {
        Ok(elapsed) => elapsed,
        Err(err) => {
            log.bug(format!(
                "BUG: Elapsed time error: {}. Contact the developers.",
                err
            ));
            return;
        }
    };
    if elapsed > server.report_hashrate_interval {
        let num_nonces = server.metrics_nonces.load(Ordering::Acquire);
        let hashrate = num_nonces as f64 / elapsed.as_secs_f64();
        log.report_hashrate(hashrate);
        server.metrics_nonces.store(0, Ordering::Release);
        *timestamp = SystemTime::now();
    }
}

async fn init_request(server: &Server) -> RequestBuilder {
    let node_settings = server.node_settings.lock().await;
    server.client.post(&node_settings.bitcoind_url).basic_auth(
        &node_settings.bitcoind_user,
        Some(&node_settings.bitcoind_password),
    )
}

fn display_hash(hash: &[u8]) -> String {
    let mut hash = hash.to_vec();
    hash.reverse();
    hex::encode(&hash)
}

async fn update_next_block(server: &Server) -> Result<(), Box<dyn std::error::Error>> {
    let log = server.log();
    let response = init_request(&server)
        .await
        .body(format!(
            r#"{{"method":"getrawunsolvedblock","params":["{}"]}}"#,
            server.node_settings.lock().await.miner_addr
        ))
        .send()
        .await?;
    let status = response.status();
    let response_str = response.text().await?;
    let response: Result<GetRawUnsolvedBlockResponse, _> = serde_json::from_str(&response_str);
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            log.error(format!(
                "getrawunsolvedblock failed ({}): {}",
                status, response_str
            ));
            if status == StatusCode::UNAUTHORIZED {
                log.error("It seems you specified the wrong username/password");
            }
            return Ok(());
        }
    };
    let mut block_state = server.block_state.lock().await;
    let unsolved_block = match response.result {
        Some(unsolved_block) => unsolved_block,
        None => {
            log.error(format!(
                "getrawunsolvedblock failed: {}",
                response.error.unwrap_or("unknown error".to_string())
            ));
            return Ok(());
        }
    };
    let block = create_block(&unsolved_block);
    if let Some(current_block) = &block_state.current_block {
        if current_block.prev_hash() != block.prev_hash() {
            log.info(format!(
                "Switched to new chain tip: {}",
                display_hash(&block.prev_hash())
            ));
        }
    } else {
        log.info(format!(
            "Started mining on chain tip: {}",
            display_hash(&block.prev_hash())
        ));
    }
    block_state.extra_nonce += 1;
    block_state.next_block = Some(block);
    Ok(())
}

async fn mine_some_nonces(server: ServerRef) -> Result<()> {
    let log = server.log();
    let mut block_state = server.block_state.lock().await;
    if let Some(next_block) = block_state.next_block.take() {
        block_state.current_work = Work::from_header(next_block.header, next_block.target);
        block_state.current_block = Some(next_block);
    }
    if block_state.current_block.is_none() {
        return Ok(());
    }
    let mut work = block_state.current_work;
    let big_nonce = server.rng.lock().await.gen();
    work.set_big_nonce(big_nonce);
    drop(block_state);
    let (nonce, num_nonces_per_search) = tokio::task::spawn_blocking({
        let server = Arc::clone(&server);
        move || {
            let log = server.log();
            let mut miner = server.miner.lock().unwrap();
            if !miner.has_nonces_left(&work) {
                log.error(format!(
                    "Error: Exhaustively searched nonces. This could be fixed by lowering \
                           rpc_poll_interval."
                ));
                return Ok((None, 0));
            }
            miner
                .find_nonce(&work, server.log())
                .map(|nonce| (nonce, miner.num_nonces_per_search()))
        }
    })
    .await
    .unwrap()?;
    let mut block_state = server.block_state.lock().await;
    if let Some(nonce) = nonce {
        work.set_big_nonce(nonce);
        log.info(format!("Block hash below target with nonce: {}", nonce));
        if let Some(mut block) = block_state.current_block.take() {
            block.header = *work.header();
            if let Err(err) = submit_block(&server, &block).await {
                log.error(format!(
                    "submit_block error: {:?}. This could be a connection issue.",
                    err
                ));
            }
        } else {
            log.bug("BUG: Found nonce but no block! Contact the developers.");
        }
    }
    block_state.current_work.nonce_idx += 1;
    server
        .metrics_nonces
        .fetch_add(num_nonces_per_search, Ordering::AcqRel);
    update_hashrate(server.as_ref(), log).await;
    Ok(())
}

async fn submit_block(server: &Server, block: &Block) -> Result<(), Box<dyn std::error::Error>> {
    #[derive(Deserialize)]
    struct SubmitBlockResponse {
        result: Option<String>,
    }
    let log = server.log();
    let mut serialized_block = block.header.to_vec();
    serialized_block.extend_from_slice(&block.body);
    let response = init_request(server)
        .await
        .body(format!(
            r#"{{"method":"submitblock","params":[{:?}]}}"#,
            hex::encode(&serialized_block)
        ))
        .send()
        .await?;
    let response: SubmitBlockResponse = serde_json::from_str(&response.text().await?)?;
    match response.result {
        None => log.info("BLOCK ACCEPTED!"),
        Some(reason) => {
            log.error(format!("REJECTED BLOCK: {}", reason));
            if reason == "inconclusive" {
                log.warn(
                    "This is an orphan race; might be fixed by lowering rpc_poll_interval or \
                          updating to the newest lotus-gpu-miner.",
                );
            } else {
                log.error(
                    "Something is misconfigured; make sure you run the latest \
                          lotusd/Lotus-QT and lotus-gpu-miner.",
                );
            }
        }
    }
    Ok(())
}

impl Log {
    pub fn new() -> Self {
        Log {
            logs: std::sync::RwLock::new(Vec::new()),
            hashrates: std::sync::RwLock::new(Vec::new()),
        }
    }

    pub fn log(&self, entry: impl Into<LogEntry>) {
        let mut logs = self.logs.write().unwrap();
        let entry = entry.into();
        println!("{}", entry);
        logs.push(entry);
    }

    pub fn log_str(&self, msg: impl ToString, severity: LogSeverity) {
        self.log(LogEntry {
            msg: msg.to_string(),
            severity,
            timestamp: chrono::Local::now(),
        })
    }

    pub fn info(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info)
    }

    pub fn warn(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Warn)
    }

    pub fn error(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Error)
    }

    pub fn bug(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Bug)
    }

    pub fn get_logs_and_clear(&self) -> Vec<LogEntry> {
        let mut logs = self.logs.write().unwrap();
        logs.drain(..).collect()
    }

    pub fn report_hashrate(&self, hashrate: f64) {
        let mut hashrates = self.hashrates.write().unwrap();
        hashrates.push(HashrateEntry {
            hashrate,
            timestamp: chrono::Local::now(),
        });
    }

    pub fn hashrates<'a>(&'a self) -> std::sync::RwLockReadGuard<'a, Vec<HashrateEntry>> {
        self.hashrates.read().unwrap()
    }
}

impl Display for LogEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} [{:?}] {}",
            self.timestamp.to_rfc3339(),
            self.severity,
            self.msg
        )
    }
}

impl Display for HashrateEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} Hashrate {:.3} MH/s",
            self.timestamp.to_rfc3339(),
            self.hashrate / 1_000_000.0
        )
    }
}
