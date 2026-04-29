# 03. Runtime logging and observability

Operator-visible logs must clearly report:

## Startup and mode
- selected mode: solo vs stratum
- selected endpoints
- GPU index/intensity

## Stratum connectivity
- connect attempt
- subscribe sent/ack
- authorize sent/ack
- disconnect reason
- reconnect backoff schedule

## Work lifecycle
- work changed (new notify)
- clean_jobs indicator
- difficulty updates (`mining.set_difficulty`)
- active job id changes

## Mining and submit lifecycle
- candidate found
- share submit sent (id/job)
- share accepted/rejected with reason
- stale job / invalid-submit diagnostics

## Performance
- hashrate interval logs
- optional counters for accepted/rejected shares

## Log severity
- INFO: operational lifecycle events
- WARN: recoverable runtime issues
- ERROR: submit/session failures
- BUG: invariant violations
