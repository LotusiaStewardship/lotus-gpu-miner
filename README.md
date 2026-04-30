# Lotus GPU Miner

`lotus-gpu-miner` is a Rust/OpenCL Lotus mining client with two runtime modes:

1. **Solo mode (existing behavior)** via direct lotusd JSON-RPC
2. **Stratum mode (new)** via Stratum V1 (`host:port`) against `stratum-server-nng`

The miner retains full solo functionality while adding Stratum compatibility.

---

## Runtime mode selection

Mode is selected by configuration/CLI values:

- If `stratum_url` is empty/missing → **solo JSON-RPC mode**
- If `stratum_url` is set (non-empty `host:port`) → **stratum mode**

No explicit `mode` field is required; the URL presence decides mode.

---

## Configuration

Default config path:

```text
~/.lotus-miner/config.toml
```

All existing solo config options remain valid and unchanged.

### Example config (backward-compatible, with new optional Stratum fields)

```toml
mine_to_address = "lotus_16PSJ..."
rpc_url = "http://127.0.0.1:10605"
rpc_poll_interval = 3
rpc_user = "lotus"
rpc_password = "lotus"
gpu_index = 0
kernel_size = 23

# New optional stratum settings
stratum_url = ""                  # host:port only, e.g. 127.0.0.1:3334
stratum_worker_name = ""          # worker suffix only (without address)
stratum_password = "x"            # passed to mining.authorize
```

### CLI options

Use `lotus-miner-cli --help` for all options.

Relevant Stratum options:

- `--stratum-url <host:port>`
- `--stratum-worker-name <worker>`
- `--stratum-password <password>`

Worker identity sent to Stratum is composed as:

```text
<mine_to_address>[.<stratum_worker_name>]
```

This matches `stratum-server-nng` requirements.

---

## Solo mode behavior

Solo mode is unchanged:

- polls lotusd with `getrawunsolvedblock`
- mines candidate nonces on GPU
- submits full block with `submitblock`

Enable solo mode by leaving `stratum_url` empty.

---

## Stratum mode behavior

When `stratum_url` is set:

1. Connect to Stratum over TCP (`host:port`)
2. Send:
   - `mining.subscribe`
   - `mining.authorize`
3. Receive and handle:
   - `lotus.precomputed_work` (`share_target_hex` is server big-endian and normalized internally)
   - `mining.set_difficulty`

`lotus.precomputed_work` is strictly validated as:
- params length exactly `6`
- `job_id`: non-empty string
- `header_160_hex`: exactly 320 hex chars (160 bytes)
- `share_target_hex`: exactly 64 hex chars (32 bytes, big-endian on wire)
- `extranonce2_hex`: exactly 8 hex chars
- `ntime_hex_6b`: exactly 12 hex chars
- `clean_jobs`: boolean
4. Mine shares with OpenCL directly on server-precomputed work
5. Submit shares using `mining.submit`

In Stratum mode, the miner must consume `lotus.precomputed_work` and does not build headers locally.
Header construction is solo-mode-only.

### Reconnect policy

On disconnect/error the miner retries forever with exponential backoff:

- 1s, 2s, 4s, ... up to max 60s

---

## Optional Stratum methods

Scaffolding and documentation are present for:

- `mining.extranonce.subscribe`
- `mining.suggest_difficulty`
- `mining.set_extranonce`

Current default behavior does **not** invoke these methods.

---

## Logging and runtime diagnostics

The miner logs:

- selected runtime mode (solo/stratum)
- Stratum connect/authorize/subscribe lifecycle
- new precomputed work jobs and difficulty updates
- candidate share finds and submit attempts
- reconnect/backoff behavior
- hashrate report lines

CLI prints hashrate every 10 seconds by default.

---

## CLI-first rollout note

This milestone focuses on **CLI-first Stratum support**.
GUI wiring can be extended afterward while preserving the same runtime model.

---

## Build & Run

## CLI (default target)

```bash
cargo run -p lotus-miner-cli -- --mine-to-address <addr> --rpc-user lotus --rpc-password lotus
```

### Stratum example

```bash
cargo run -p lotus-miner-cli -- \
  --mine-to-address <lotus_address> \
  --stratum-url 127.0.0.1:3334 \
  --stratum-worker-name rig01 \
  --stratum-password x \
  --gpu-index 0 \
  --kernel-size 23
```

---

## Development checks

```bash
cargo check --workspace
```
