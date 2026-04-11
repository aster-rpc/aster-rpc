# hello-world

The simplest possible Aster service in TypeScript. One file. No network.
A unary RPC and a server-streaming RPC running entirely in-process via
`LocalTransport`.

```bash
bun service.ts
```

```
--- Unary ---
Hello, TypeScript!

--- Server Stream ---
Aster: 3...
Aster: 2...
Aster: 1...
Aster: Go!

Done!
```

This example uses `LocalTransport` -- no QUIC, no credentials, no
`aster1...` ticket. Both the producer and the consumer live in the same
process. It is the cheapest possible way to verify that your install
works and to play with the decorator surface.

For the real-network version (across machines, with credentials, the
full streaming and session story), work through the
[Mission Control walkthrough](https://docs.aster.site/docs/quickstart/mission-control).
