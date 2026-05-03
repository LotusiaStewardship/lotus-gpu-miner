# 04. Testing, validation, rollout

## Unit-level checks
- config mode switching tests
- worker full-name assembly tests
- stratum JSON parsing tests

## Integration checks
- run `stratum-server-nng`
- connect miner in stratum mode
- verify subscribe/authorize handshake
- verify notify and difficulty updates are logged
- verify submits are emitted and responses logged

## End-to-end acceptance criteria
- miner receives work and mines continuously
- shares are submitted and accepted/rejected deterministically
- reconnection works after server restart
- solo mode still operates unchanged

## Rollout order
1. CLI mode stabilization (required)
2. GUI stratum settings integration
3. operator docs finalization
