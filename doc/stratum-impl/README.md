# lotus-gpu-miner Stratum implementation plan

This plan defines client-side Stratum V1 integration for `lotus-gpu-miner` with `stratum-server-nng` while preserving existing solo mining behavior.

Primary goals:
- keep backward-compatible solo JSON-RPC mining
- add full Stratum V1 client compliance for server methods in use
- add strong runtime observability expected by miners/operators
- ensure end-to-end share submit/accept flow works with `stratum-server-nng`

Plan files:
- `01-requirements-and-compat.md`
- `02-protocol-client-phases.md`
- `03-runtime-logging-and-observability.md`
- `04-testing-validation-rollout.md`
