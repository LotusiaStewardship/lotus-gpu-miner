# 02. Protocol client phases

## P0 - Foundation and config
- add stratum config fields
- preserve solo defaults
- runtime mode switch based on `stratum_url`

## P1 - Stratum session core
- open TCP connection to `stratum_url`
- send `mining.subscribe`
- send `mining.authorize`
- parse JSON line responses safely
- reconnect loop with exponential backoff

## P2 - Work update handling
- process `mining.notify`
- process `mining.set_difficulty`
- update internal work state and active job id
- reset nonce search state on clean job updates

## P3 - Share submit path
- mine nonce candidate from OpenCL path
- submit with `mining.submit(worker, job_id, extranonce2, ntime, nonce)`
- parse submit responses
- classify accepted/rejected/errored shares

## P4 - Full runtime compliance and edge handling
- stale job handling
- duplicate response handling tolerance
- line framing and malformed frame diagnostics
- optional-method scaffolding documented and guarded

## P5 - GUI follow-up
- CLI-first completed first
- GUI form fields for stratum settings
- GUI mode visibility and warnings
