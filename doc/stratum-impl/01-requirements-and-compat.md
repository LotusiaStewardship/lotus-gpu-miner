# 01. Requirements and compatibility

## Runtime mode selection

Mode is inferred from config/CLI:
- `stratum_url` empty -> solo JSON-RPC mode
- `stratum_url` set (`host:port`) -> Stratum mode

No existing solo settings are removed or renamed.

## Required Stratum config

- `mine_to_address` (required identity base)
- `stratum_url` (host:port only; DNS or IP host)
- `stratum_worker_name` (optional suffix)
- `stratum_password` (must be passed via CLI)

Full worker identity sent to pool:
`<mine_to_address>[.<stratum_worker_name>]`

## Compatibility with stratum-server-nng

Client must implement:
- `mining.subscribe`
- `mining.authorize`
- `mining.submit`
- `mining.ping` handling compatibility
- `mining.notify` consumption
- `mining.set_difficulty` consumption

Optional method scaffolding (doc + code hooks):
- `mining.extranonce.subscribe`
- `mining.suggest_difficulty`
- `mining.set_extranonce`

## Reliability requirements

- infinite reconnect retry
- exponential backoff
- max backoff 60s

## Backward compatibility

Existing `config.toml` files for solo mining remain valid without changes.
