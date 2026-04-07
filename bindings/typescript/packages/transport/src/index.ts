/**
 * @aster-rpc/transport — Iroh P2P transport bindings for TypeScript.
 *
 * This package provides native Node.js bindings to the Iroh peer-to-peer
 * networking library via NAPI-RS. It exposes QUIC networking, content-addressed
 * blob storage, CRDT documents, and gossip pub-sub.
 *
 * Works with Node.js 20+, Bun 1.0+, and Deno (via Node compat).
 *
 * @packageDocumentation
 */

// Native bindings will be re-exported here once the NAPI-RS crate is built.
// For now, export the transport interface that Layer 2 depends on.

export { type SendStream, type RecvStream } from './transport.js';
