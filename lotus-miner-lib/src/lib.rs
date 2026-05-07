mod block;
mod miner;
pub mod settings;
mod sha256;

use colored::Colorize;
use eyre::Result;
pub use miner::Miner;
pub use settings::ConfigSettings;

use std::{
    collections::HashMap,
    convert::TryInto,
    fmt::Display,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

use bitcoinsuite_bitcoind_stratum::{build_stratum_header, difficulty_to_target};
use bitcoinsuite_core::LotusAddress;
use block::{create_block, Block, GetRawUnsolvedBlockResponse};
use miner::{MiningSettings, Work};
use rand::{Rng, SeedableRng};
use reqwest::{RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::Value;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStyle {
    Status,      // Cyan - connection/session state
    Work,        // White - job updates
    Difficulty,  // Magenta - difficulty changes
    ShareAccepted,   // Green
    ShareRejected, // Yellow
    ShareErrored,  // Red
    Hashrate,    // Blue
    Block,       // Bright Green + Bold
    Warn,        // Yellow
    Error,       // Red
    Bug,         // Bright Red
    Debug,       // Gray
}

pub struct Log {
    logs: std::sync::RwLock<Vec<LogEntry>>,
    hashrates: std::sync::RwLock<Vec<HashrateEntry>>,
    no_color: bool,
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
    pub style: LogStyle,
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
    ntime_hex_6b: String,
    notify: ParsedNotify,
    share_target_le: [u8; 32],
    extranonce2_counter: u64,
    extranonce2_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingRequestKind {
    Subscribe,
    Authorize,
    Submit {
        job_id: String,
        extranonce2: String,
        ntime_hex_6b: String,
        nonce_hex_8b: String,
    },
    Ping,
}

#[derive(Debug, Default)]
struct StratumShareStats {
    accepted: u64,
    rejected: u64,
    errored: u64,
    total_submitted: u64,
}

#[derive(Debug, Default)]
struct StratumHandshakeState {
    subscribed: bool,
    authorized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedNotify {
    job_id: String,
    prevhash: String,
    coinbase1: String,
    coinbase2: String,
    merkle_branches: Vec<String>,
    version: String,
    nbits: String,
    ntime_hex_6b: String,
    clean_jobs: bool,
    // Lotus-specific extensions
    block_height: i32,
    epoch_hash_hex: String,
    extended_metadata_hash_hex: String,
    block_size: u64,
}

fn is_valid_lotus_identity(s: &str) -> bool {
    s.parse::<LotusAddress>().is_ok()
}

fn parse_notify_params(params: &[Value]) -> Result<ParsedNotify> {
    // Standard stratum params (9) + Lotus extensions (3: height, epoch_hash, extended_metadata_hash)
    if params.len() < 9 {
        return Err(eyre::eyre!("mining.notify params must have length >= 9"));
    }
    let job_id = params[0]
        .as_str()
        .ok_or_else(|| eyre::eyre!("invalid job_id in mining.notify"))?
        .to_string();
    let prevhash = params[1]
        .as_str()
        .ok_or_else(|| eyre::eyre!("invalid prevhash in mining.notify"))?
        .to_string();
    let coinbase1 = params[2]
        .as_str()
        .ok_or_else(|| eyre::eyre!("invalid coinbase1 in mining.notify"))?
        .to_string();
    let coinbase2 = params[3]
        .as_str()
        .ok_or_else(|| eyre::eyre!("invalid coinbase2 in mining.notify"))?
        .to_string();
    let merkle_branches = params[4]
        .as_array()
        .ok_or_else(|| eyre::eyre!("invalid merkle branches in mining.notify"))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| eyre::eyre!("invalid merkle branch in mining.notify"))
                .map(|s| s.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    let version = params[5]
        .as_str()
        .ok_or_else(|| eyre::eyre!("invalid version in mining.notify"))?
        .to_string();
    let nbits = params[6]
        .as_str()
        .ok_or_else(|| eyre::eyre!("invalid nbits in mining.notify"))?
        .to_string();
    let ntime_hex_6b = params[7]
        .as_str()
        .ok_or_else(|| eyre::eyre!("invalid ntime in mining.notify"))?
        .to_string();
    let clean_jobs = params[8]
        .as_bool()
        .ok_or_else(|| eyre::eyre!("invalid clean_jobs in mining.notify"))?;

    // Parse Lotus-specific extensions (optional, default to 0/zero-hash for backward compat)
    let block_height = params
        .get(9)
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .unwrap_or(0);
    let epoch_hash_hex = params
        .get(10)
        .and_then(|v| v.as_str())
        .unwrap_or("0000000000000000000000000000000000000000000000000000000000000000")
        .to_string();
    let extended_metadata_hash_hex = params
        .get(11)
        .and_then(|v| v.as_str())
        .unwrap_or("0000000000000000000000000000000000000000000000000000000000000000")
        .to_string();
    let block_size = params.get(12).and_then(|v| v.as_u64()).unwrap_or(0);

    Ok(ParsedNotify {
        job_id,
        prevhash,
        coinbase1,
        coinbase2,
        merkle_branches,
        version,
        nbits,
        ntime_hex_6b,
        clean_jobs,
        block_height,
        epoch_hash_hex,
        extended_metadata_hash_hex,
        block_size,
    })
}

fn format_extranonce2(counter: u64, extranonce2_size: usize) -> Result<String> {
    if extranonce2_size == 0 || extranonce2_size > 8 {
        return Err(eyre::eyre!(
            "unsupported extranonce2_size {}; expected 1..=8",
            extranonce2_size
        ));
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&counter.to_be_bytes());
    Ok(hex::encode(&bytes[8 - extranonce2_size..]))
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
            log: Log::new(config.no_color),
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
        if !is_valid_lotus_identity(&miner_addr) {
            return Err(eyre::eyre!(
                "stratum mode requires mine_to_address in lotus_* format"
            ));
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
            .status(format!("Connecting to pool: {}", stratum_url));

        match run_stratum_session(Arc::clone(&server), &stratum_url).await {
            Ok(()) => {
                backoff_secs = 1;
                server.log().warn("Pool connection lost, reconnecting");
            }
            Err(err) => {
                server
                    .log()
                    .error(format!("Session error: {}", err));
            }
        }

        server
            .log()
            .status(format!("Reconnecting in {}s", backoff_secs));
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
    let mut pending: HashMap<u64, PendingRequestKind> = HashMap::new();
    let mut share_stats = StratumShareStats::default();
    let mut handshake = StratumHandshakeState::default();
    let mut share_timestamps: HashMap<u64, std::time::Instant> = HashMap::new();

    let worker_name = server.stratum_worker_full_name().await?;
    let stratum_password = server
        .stratum_settings
        .lock()
        .await
        .stratum_password
        .clone();

    let subscribe_id = req_id;
    req_id += 1;
    pending.insert(subscribe_id, PendingRequestKind::Subscribe);
    let subscribe = serde_json::json!({
        "id": subscribe_id,
        "method": "mining.subscribe",
        "params": []
    });
    writer_half
        .write_all(format!("{}\n", subscribe).as_bytes())
        .await?;
    server.log().debug("Sent mining.subscribe");

    let mut authorize_sent = false;
    let mut last_outbound = std::time::Instant::now();
    loop {
        line.clear();
        tokio::select! {
            read = reader.read_line(&mut line) => {
                let n = read?;
                if n == 0 {
                    return Err(eyre::eyre!("stratum socket closed"));
                }
                if line.len() > 8192 {
                    return Err(eyre::eyre!("stratum line exceeds 8192 bytes"));
                }
                handle_stratum_line(
                    &server,
                    line.trim_end(),
                    &mut pending,
                    &mut share_stats,
                    &mut handshake,
                    &mut share_timestamps,
                    &worker_name,
                )
                .await?;
                if handshake.subscribed && !authorize_sent {
                    let authorize_id = req_id;
                    req_id += 1;
                    pending.insert(authorize_id, PendingRequestKind::Authorize);
                    let authorize = serde_json::json!({
                        "id": authorize_id,
                        "method": "mining.authorize",
                        "params": [worker_name, stratum_password]
                    });
                    writer_half
                        .write_all(format!("{}\n", authorize).as_bytes())
                        .await?;
                    authorize_sent = true;
                    last_outbound = std::time::Instant::now();
                    server.log().debug("Sent mining.authorize");
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                if handshake.authorized {
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
                        pending.insert(submit_id, PendingRequestKind::Submit {
                            job_id,
                            extranonce2,
                            ntime_hex_6b: ntime,
                            nonce_hex_8b: nonce_hex,
                        });
                        share_timestamps.insert(submit_id, std::time::Instant::now());
                        share_stats.total_submitted += 1;
                        last_outbound = std::time::Instant::now();
                    } else if last_outbound.elapsed() >= Duration::from_secs(30) {
                        let ping_id = req_id;
                        req_id += 1;
                        let ping = serde_json::json!({
                            "id": ping_id,
                            "method": "mining.ping",
                            "params": []
                        });
                        writer_half
                            .write_all(format!("{}\n", ping).as_bytes())
                            .await?;
                        pending.insert(ping_id, PendingRequestKind::Ping);
                        last_outbound = std::time::Instant::now();
                        server.log().debug(format!("Sent mining.ping id={}", ping_id));
                    }
                }
            }
        }
    }
}

async fn handle_stratum_line(
    server: &Server,
    line: &str,
    pending: &mut HashMap<u64, PendingRequestKind>,
    share_stats: &mut StratumShareStats,
    handshake: &mut StratumHandshakeState,
    share_timestamps: &mut HashMap<u64, std::time::Instant>,
    worker_name: &str,
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
                    .unwrap();
                server.stratum_settings.lock().await.stratum_difficulty = diff;
                server.log().difficulty(format!("Difficulty set: {}", diff));
            }
            "mining.set_extranonce" => {
                let params = v
                    .get("params")
                    .and_then(|p| p.as_array())
                    .ok_or_else(|| eyre::eyre!("invalid mining.set_extranonce params"))?;
                if params.len() != 2 {
                    return Err(eyre::eyre!(
                        "mining.set_extranonce params must have length 2"
                    ));
                }
                let extranonce1 = params[0]
                    .as_str()
                    .ok_or_else(|| eyre::eyre!("invalid extranonce1 in mining.set_extranonce"))?
                    .to_string();
                let extranonce2_size = params[1].as_u64().ok_or_else(|| {
                    eyre::eyre!("invalid extranonce2_size in mining.set_extranonce")
                })? as usize;
                let mut settings = server.stratum_settings.lock().await;
                settings.stratum_extranonce1 = extranonce1.clone();
                settings.stratum_extranonce2_size = extranonce2_size;
                server.log().debug(format!(
                    "set_extranonce: extranonce1={} size={}",
                    extranonce1, extranonce2_size
                ));
            }
            "mining.notify" => {
                if !handshake.subscribed {
                    return Err(eyre::eyre!("received mining.notify before subscribe"));
                }
                if !handshake.authorized {
                    server.log().debug("received mining.notify before authorize; queueing");
                }
                let params = v
                    .get("params")
                    .and_then(|p| p.as_array())
                    .ok_or_else(|| eyre::eyre!("invalid mining.notify params"))?;
                let parsed = parse_notify_params(params)?;
                let settings = server.stratum_settings.lock().await;
                let extranonce1 = settings.stratum_extranonce1.clone();
                // difficulty_to_target returns big-endian, but Work expects LE
                let target_be = difficulty_to_target(settings.stratum_difficulty)?;
                let mut share_target_le = [0u8; 32];
                share_target_le.copy_from_slice(&target_be);
                share_target_le.reverse(); // Convert BE to LE for Work
                let extranonce2_size = settings.stratum_extranonce2_size;
                drop(settings);

                let extranonce2 = format_extranonce2(0, extranonce2_size)?;

                let header_vec = build_stratum_header(
                    &parsed.coinbase1,
                    &extranonce1,
                    &extranonce2,
                    &parsed.coinbase2,
                    &parsed.merkle_branches,
                    &parsed.prevhash,
                    &parsed.version,
                    &parsed.nbits,
                    &parsed.ntime_hex_6b,
                    "0000000000000000",
                    Some(parsed.block_height),
                    Some(&parsed.epoch_hash_hex),
                    Some(&parsed.extended_metadata_hash_hex),
                    Some(parsed.block_size),
                )?;
                let header_160: [u8; 160] = header_vec
                    .as_slice()
                    .try_into()
                    .map_err(|_| eyre::eyre!("invalid header length"))?;

                let mut block_state = server.block_state.lock().await;
                block_state.current_work = Work::from_header(header_160, share_target_le);
                if parsed.clean_jobs {
                    block_state.current_work.nonce_idx = 0;
                }
                let parsed_job_id = parsed.job_id.clone();
                let parsed_clean_jobs = parsed.clean_jobs;
                let parsed_block_height = parsed.block_height;
                block_state.stratum_job = Some(StratumJob {
                    job_id: parsed_job_id.clone(),
                    ntime_hex_6b: parsed.ntime_hex_6b.clone(),
                    notify: parsed.clone(),
                    share_target_le,
                    extranonce2_counter: 0,
                    extranonce2_size,
                });
                drop(block_state);
                let clean_str = if parsed_clean_jobs { "yes" } else { "no" };
                let height_str = if parsed_block_height > 0 {
                    format!(" height:{}", parsed_block_height)
                } else {
                    String::new()
                };
                server.log().work(format!(
                    "New job #{}{} | clean: {}", parsed_job_id, height_str, clean_str
                ));
            }
            _ => {}
        }
        return Ok(());
    }

    let id = v.get("id").and_then(|id| id.as_u64()).unwrap_or(0);
    let kind = pending.remove(&id);
    let err = v.get("error");

    match kind {
        Some(PendingRequestKind::Subscribe) => {
            if err.is_some() && !err.is_none_or(|e| e.is_null()) {
                return Err(eyre::eyre!("mining.subscribe failed: {}", err.unwrap()));
            }
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
            handshake.subscribed = true;
            server.log().status(format!("Subscribed ✓ | extranonce2_size: {}", extranonce2_size));
        }
        Some(PendingRequestKind::Authorize) => {
            if err.is_some() && !err.is_none_or(|e| e.is_null()) {
                return Err(eyre::eyre!("mining.authorize failed: {}", err.unwrap()));
            }
            let ok = v.get("result").and_then(|r| r.as_bool()).unwrap_or(false);
            if ok {
                handshake.authorized = true;
                server.log().status(format!("Authorized ✓ | worker: {}", worker_name));
            } else {
                return Err(eyre::eyre!("mining.authorize rejected by server"));
            }
        }
        Some(PendingRequestKind::Submit {
            job_id,
            extranonce2: _,
            ntime_hex_6b: _,
            nonce_hex_8b: _,
        }) => {
            let share_num = share_stats.total_submitted;
            let latency_ms = share_timestamps
                .remove(&id)
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);

            if err.is_some() && !err.is_none_or(|e| e.is_null()) {
                share_stats.errored += 1;
                let err_msg = err.unwrap().to_string();
                let reason = if err_msg.contains("Low difficulty") { "low_difficulty"
                    } else if err_msg.contains("stale") { "stale_job"
                    } else if err_msg.contains("duplicate") { "duplicate_share"
                    } else { "unknown" };
                let totals = format!("total: {}✓ / {}✗ / {}!",
                    share_stats.accepted, share_stats.rejected, share_stats.errored);
                server.log().share_errored(format!(
                    "#{} errored | job:{} | reason:{} | {}ms | {}",
                    share_num, job_id, reason, latency_ms, totals
                ));
            } else if let Some(result_bool) = v.get("result").and_then(|r| r.as_bool()) {
                if result_bool {
                    share_stats.accepted += 1;
                    let totals = format!("total: {}✓ / {}✗ / {}!",
                        share_stats.accepted, share_stats.rejected, share_stats.errored);
                    server.log().share_accepted(format!(
                        "#{} accepted | job:{} | {}ms | {}",
                        share_num, job_id, latency_ms, totals
                    ));
                } else {
                    share_stats.rejected += 1;
                    let totals = format!("total: {}✓ / {}✗ / {}!",
                        share_stats.accepted, share_stats.rejected, share_stats.errored);
                    server.log().share_rejected(format!(
                        "#{} rejected | job:{} | {}ms | {}",
                        share_num, job_id, latency_ms, totals
                    ));
                }
            } else {
                share_stats.errored += 1;
                let totals = format!("total: {}✓ / {}✗ / {}!",
                    share_stats.accepted, share_stats.rejected, share_stats.errored);
                server.log().share_errored(format!(
                    "#{} invalid_response | job:{} | {}ms | {}",
                    share_num, job_id, latency_ms, totals
                ));
            }
        }
        Some(PendingRequestKind::Ping) => {
            if err.is_some() && !err.is_none_or(|e| e.is_null()) {
                server.log().warn(format!(
                    "mining.ping error id={} error={}",
                    id,
                    err.unwrap()
                ));
            }
        }
        None => {
            if err.is_some() && !err.is_none_or(|e| e.is_null()) {
                server.log().warn(format!(
                    "Uncorrelated stratum response id={} error={}",
                    id,
                    err.unwrap()
                ));
            }
        }
    }

    Ok(())
}

async fn mine_some_nonces_stratum(
    server: ServerRef,
) -> Result<Option<(String, String, String, String)>> {
    let log = server.log();
    let mut block_state = server.block_state.lock().await;
    let Some(mut job) = block_state.stratum_job.clone() else {
        return Ok(None);
    };

    let extranonce2 = format_extranonce2(job.extranonce2_counter, job.extranonce2_size)?;
    let settings = server.stratum_settings.lock().await;
    let extranonce1 = settings.stratum_extranonce1.clone();
    drop(settings);

    let header_vec = build_stratum_header(
        &job.notify.coinbase1,
        &extranonce1,
        &extranonce2,
        &job.notify.coinbase2,
        &job.notify.merkle_branches,
        &job.notify.prevhash,
        &job.notify.version,
        &job.notify.nbits,
        &job.notify.ntime_hex_6b,
        "0000000000000000",
        Some(job.notify.block_height),
        Some(&job.notify.epoch_hash_hex),
        Some(&job.notify.extended_metadata_hash_hex),
        Some(job.notify.block_size),
    )?;
    let header_160: [u8; 160] = header_vec
        .as_slice()
        .try_into()
        .map_err(|_| eyre::eyre!("invalid header length"))?;
    block_state.current_work = Work::from_header(header_160, job.share_target_le);
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
    job.extranonce2_counter = job.extranonce2_counter.wrapping_add(1);
    block_state.stratum_job = Some(job.clone());
    server
        .metrics_nonces
        .fetch_add(num_nonces_per_search, Ordering::AcqRel);
    update_hashrate(server.as_ref(), log).await;

    if let Some(nonce) = nonce {
        // Submit nonce as the exact 8 header bytes in little-endian order.
        let nonce_hex = hex::encode(nonce.to_le_bytes());
        log.debug(format!(
            "Candidate share job_id={} extranonce2={} nonce_le={}",
            job.job_id, extranonce2, nonce_hex
        ));
        return Ok(Some((job.job_id, extranonce2, job.ntime_hex_6b, nonce_hex)));
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
        log.hashrate(format!("{:.2} MH/s", hashrate / 1_000_000.0));
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
    pub fn new(no_color: bool) -> Self {
        Log {
            logs: std::sync::RwLock::new(Vec::new()),
            hashrates: std::sync::RwLock::new(Vec::new()),
            no_color,
        }
    }

    pub fn log(&self, entry: impl Into<LogEntry>) {
        let mut logs = self.logs.write().unwrap();
        let entry = entry.into();
        println!("{}", entry.display(self.no_color));
        logs.push(entry);
    }

    pub fn log_str(&self, msg: impl ToString, severity: LogSeverity, style: LogStyle) {
        self.log(LogEntry {
            msg: msg.to_string(),
            severity,
            timestamp: chrono::Local::now(),
            style,
        })
    }

    pub fn info(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::Status)
    }

    pub fn warn(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Warn, LogStyle::Warn)
    }

    pub fn error(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Error, LogStyle::Error)
    }

    pub fn bug(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Bug, LogStyle::Bug)
    }

    // Styled logging methods
    pub fn status(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::Status)
    }

    pub fn work(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::Work)
    }

    pub fn difficulty(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::Difficulty)
    }

    pub fn share_accepted(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::ShareAccepted)
    }

    pub fn share_rejected(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Warn, LogStyle::ShareRejected)
    }

    pub fn share_errored(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Warn, LogStyle::ShareErrored)
    }

    pub fn hashrate(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::Hashrate)
    }

    pub fn block(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::Block)
    }

    pub fn debug(&self, msg: impl ToString) {
        self.log_str(msg, LogSeverity::Info, LogStyle::Debug)
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

impl LogEntry {
    pub fn display(&self, no_color: bool) -> String {
        let time_str = self.timestamp.format("%H:%M:%S").to_string();
        let style_str = self.style_label();
        let msg = &self.msg;

        let formatted = format!("[{}] [{}] {}", time_str, style_str, msg);

        if no_color {
            formatted
        } else {
            match self.style {
                LogStyle::Status => formatted.cyan().to_string(),
                LogStyle::Work => formatted.bright_white().to_string(),
                LogStyle::Difficulty => formatted.truecolor(255, 0, 255).to_string(),
                LogStyle::ShareAccepted => formatted.green().to_string(),
                LogStyle::ShareRejected => formatted.yellow().to_string(),
                LogStyle::ShareErrored => formatted.red().to_string(),
                LogStyle::Hashrate => formatted.blue().to_string(),
                LogStyle::Block => formatted.bright_green().bold().to_string(),
                LogStyle::Warn => formatted.yellow().to_string(),
                LogStyle::Error => formatted.red().to_string(),
                LogStyle::Bug => formatted.bright_red().bold().to_string(),
                LogStyle::Debug => formatted.truecolor(128, 128, 128).to_string(),
            }
        }
    }

    fn style_label(&self) -> &'static str {
        match self.style {
            LogStyle::Status => "STATUS",
            LogStyle::Work => "WORK",
            LogStyle::Difficulty => "DIFF",
            LogStyle::ShareAccepted => "SHARE ✓",
            LogStyle::ShareRejected => "SHARE ✗",
            LogStyle::ShareErrored => "SHARE !",
            LogStyle::Hashrate => "HASHRATE",
            LogStyle::Block => "BLOCK",
            LogStyle::Warn => "WARN",
            LogStyle::Error => "ERROR",
            LogStyle::Bug => "BUG",
            LogStyle::Debug => "DEBUG",
        }
    }
}

impl Display for LogEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display(false))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lotus_identity_requires_prefix_and_payload() {
        assert!(is_valid_lotus_identity(
            "lotus_16PSJNf1EDEfGvaYzaXJCJZrXH4pgiTo7kyW61iGi"
        ));
        assert!(!is_valid_lotus_identity("lotus_"));
        assert!(!is_valid_lotus_identity("ecash:q..."));
    }

    #[test]
    fn parse_notify_params_validates_shape() {
        let params = vec![
            Value::String("job-1".to_string()),
            Value::String("00".repeat(32)),
            Value::String("11".repeat(10)),
            Value::String("22".repeat(10)),
            Value::Array(vec![]),
            Value::String("01000000".to_string()),
            Value::String("ffff001d".to_string()),
            Value::String("aabbccddeeff".to_string()),
            Value::Bool(true),
        ];

        let parsed = parse_notify_params(&params).unwrap();
        assert_eq!(parsed.job_id, "job-1");
        assert_eq!(parsed.ntime_hex_6b, "aabbccddeeff");
        assert!(parsed.clean_jobs);
    }

    #[test]
    fn parse_notify_params_rejects_invalid_lengths() {
        let params = vec![
            Value::String("job-1".to_string()),
            Value::String("00".repeat(32)),
        ];
        assert!(parse_notify_params(&params).is_err());
    }
}
