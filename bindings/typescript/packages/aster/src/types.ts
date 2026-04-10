/**
 * Shared types and enums for the Aster RPC framework.
 */

/** Serialization mode for Fory codec. */
export const SerializationMode = {
  /** Cross-language (Fory XLANG). Default and recommended. */
  XLANG: 0,
  /** Language-native. Only for LocalTransport (same trust domain). */
  NATIVE: 1,
  /** Row-oriented. For random-access reads. */
  ROW: 2,
  /** JSON over UTF-8. Cross-language fallback. The TypeScript binding
   *  uses this exclusively because Fory JS is not yet XLANG-compliant. */
  JSON: 3,
} as const;

export type SerializationMode = (typeof SerializationMode)[keyof typeof SerializationMode];

/** RPC streaming pattern. */
export const RpcPattern = {
  UNARY: 'unary',
  SERVER_STREAM: 'server_stream',
  CLIENT_STREAM: 'client_stream',
  BIDI_STREAM: 'bidi_stream',
} as const;

export type RpcPattern = (typeof RpcPattern)[keyof typeof RpcPattern];

/** Service dispatch scope.
 *
 *  - SHARED:  one service instance shared by all callers, fresh QUIC stream
 *             per RPC call. The default for stateless services.
 *  - SESSION: one service instance per client connection, all calls for that
 *             instance multiplexed onto a single bidirectional QUIC stream.
 *             Use this when the service needs per-peer state. The decorator
 *             still accepts the legacy alias `'stream'` on input.
 */
export const RpcScope = {
  SHARED: 'shared',
  SESSION: 'session',
} as const;

export type RpcScope = (typeof RpcScope)[keyof typeof RpcScope];

/** Exponential backoff configuration. */
export interface ExponentialBackoff {
  initialMs: number;
  maxMs: number;
  multiplier: number;
  jitter: number;
}

/** Default backoff: 100ms initial, 30s max, 2x multiplier, 10% jitter. */
export const DEFAULT_BACKOFF: ExponentialBackoff = {
  initialMs: 100,
  maxMs: 30_000,
  multiplier: 2.0,
  jitter: 0.1,
};

/** Retry policy configuration. */
export interface RetryPolicy {
  maxAttempts: number;
  backoff: ExponentialBackoff;
}

/** Default retry: 3 attempts with default backoff. */
export const DEFAULT_RETRY: RetryPolicy = {
  maxAttempts: 3,
  backoff: DEFAULT_BACKOFF,
};

/** The ALPN protocol identifier for Aster RPC. */
export const RPC_ALPN = new TextEncoder().encode('aster/1');
