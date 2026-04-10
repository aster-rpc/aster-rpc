/**
 * @aster endpoint registration loop.
 *
 * Background task that registers producer endpoints with the @aster
 * service so consumers can discover them. Mirrors the Python
 * `_aster_registration_loop` / `_register_endpoints_with_aster` /
 * `_resolve_aster_address` / `_load_producer_tokens` from runtime.py.
 *
 * Usage:
 *   const tokens = loadProducerTokens(identityData);
 *   const abort = startRegistrationLoop({ tokens, nodeInfo, logger });
 *   // later...
 *   abort.abort();
 */

import type { AsterLogger } from './logging.js';

// ── Types ────────────────────────────────────────────────────────────────────

/** Token data for a single published service (from .aster-identity). */
export interface ProducerTokenEntry {
  producer_token: string;
  [key: string]: unknown;
}

/** Node addressing information passed to the registration RPC. */
export interface NodeInfo {
  nodeId: string;
  relay: string;
  directAddrs: string[];
}

/** Options for `startRegistrationLoop`. */
export interface RegistrationLoopOptions {
  /** Map of service name -> token data (from `loadProducerTokens`). */
  tokens: Map<string, ProducerTokenEntry>;
  /** This node's addressing info. */
  nodeInfo: NodeInfo;
  /** TTL in seconds for endpoint leases. Default: 300 (5 min). */
  ttl?: number;
  /** Logger instance. */
  logger?: AsterLogger;
  /** Optional identity peer entry for resolving @aster address. */
  identityPeer?: Record<string, unknown>;
  /**
   * Factory that opens an IrohTransport to the given address.
   * The caller provides this so the registration module stays decoupled
   * from native addon loading.
   */
  connectTransport: (address: string) => Promise<RegistrationTransport>;
}

/** Minimal transport interface -- only unary is needed for registration. */
export interface RegistrationTransport {
  unary(service: string, method: string, payload: unknown): Promise<unknown>;
  close(): Promise<void>;
}

// ── loadProducerTokens ───────────────────────────────────────────────────────

/**
 * Extract producer service tokens from parsed identity data.
 *
 * In the .aster-identity TOML file, published services live under
 * `[published_services.<ServiceName>]` sections. The identity loader
 * returns the peer entry as a plain object; this function pulls out
 * entries that have a `producer_token` field.
 *
 * @param identityData - The peer entry object from the identity loader.
 * @returns Map of service name to token data.
 */
export function loadProducerTokens(
  identityData: Record<string, unknown> | null | undefined,
): Map<string, ProducerTokenEntry> {
  const tokens = new Map<string, ProducerTokenEntry>();
  if (!identityData) return tokens;

  const published = identityData.published_services;
  if (!published || typeof published !== 'object') return tokens;

  for (const [svcName, tokenData] of Object.entries(published as Record<string, unknown>)) {
    if (
      tokenData &&
      typeof tokenData === 'object' &&
      (tokenData as Record<string, unknown>).producer_token
    ) {
      tokens.set(svcName, tokenData as ProducerTokenEntry);
    }
  }

  return tokens;
}

// ── resolveAsterAddress ──────────────────────────────────────────────────────

/**
 * Resolve the @aster service address for endpoint registration.
 *
 * Checks (in order):
 * 1. `ASTER_SERVICE_ADDRESS` environment variable
 * 2. `aster_service` field in the identity peer entry
 * 3. Returns `null` if neither is set (DNS TXT lookup is future work)
 */
export function resolveAsterAddress(
  identityPeer?: Record<string, unknown> | null,
): string | null {
  // Env var override
  const envAddr = typeof process !== 'undefined'
    ? process.env.ASTER_SERVICE_ADDRESS ?? ''
    : '';
  if (envAddr) return envAddr;

  // Identity file -- the peer entry may have aster_service config
  if (identityPeer) {
    const addr = identityPeer.aster_service;
    if (typeof addr === 'string' && addr) return addr;
  }

  return null;
}

// ── registerEndpoints (one-shot) ─────────────────────────────────────────────

/**
 * One-shot registration of all published service endpoints with @aster.
 *
 * For each service with a producer token, calls
 * `PublicationService.register_endpoint` on the @aster instance.
 */
async function registerEndpoints(
  opts: RegistrationLoopOptions,
  ttl: number,
): Promise<void> {
  if (opts.tokens.size === 0) return;

  const asterAddr = resolveAsterAddress(opts.identityPeer);
  if (!asterAddr) {
    opts.logger?.debug('No @aster address configured -- skipping registration');
    return;
  }

  let transport: RegistrationTransport | null = null;
  try {
    transport = await opts.connectTransport(asterAddr);
  } catch (e) {
    opts.logger?.debug(`Could not connect to @aster: ${e}`);
    return;
  }

  try {
    for (const [svcName, tokenData] of opts.tokens) {
      try {
        const request = {
          producer_token: JSON.stringify(tokenData),
          node_id: opts.nodeInfo.nodeId,
          relay: opts.nodeInfo.relay,
          direct_addrs: opts.nodeInfo.directAddrs,
          ttl,
        };

        await transport.unary('PublicationService', 'register_endpoint', request);

        opts.logger?.info(
          `Registered endpoint with @aster: ${svcName} (${opts.nodeInfo.nodeId.slice(0, 12)})`,
        );
      } catch (e) {
        opts.logger?.warn(
          `Failed to register ${svcName} with @aster: ${e}`,
        );
      }
    }
  } finally {
    await transport.close();
  }
}

// ── startRegistrationLoop ────────────────────────────────────────────────────

/**
 * Start the background registration loop.
 *
 * Registers endpoints with @aster immediately (after a 2 s startup
 * delay), then re-registers at 75% of TTL to keep leases alive.
 *
 * Returns an `AbortController` -- call `.abort()` to stop the loop.
 */
export function startRegistrationLoop(
  opts: RegistrationLoopOptions,
): AbortController {
  const abort = new AbortController();
  const ttl = opts.ttl ?? 300;
  const intervalMs = ttl * 0.75 * 1000;

  const run = async () => {
    // Wait for the server to be fully ready
    await sleep(2000, abort.signal);
    if (abort.signal.aborted) return;

    while (!abort.signal.aborted) {
      try {
        await registerEndpoints(opts, ttl);
      } catch (e) {
        if (abort.signal.aborted) return;
        opts.logger?.info(`@aster registration failed: ${e}`);
      }

      // Wait before re-registering
      await sleep(intervalMs, abort.signal);
    }
  };

  // Fire and forget -- the caller controls lifetime via AbortController
  run().catch(() => {});

  return abort;
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/** Sleep that respects abort signal. */
function sleep(ms: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) return Promise.resolve();
  return new Promise<void>(resolve => {
    const timer = setTimeout(resolve, ms);
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    signal.addEventListener('abort', onAbort, { once: true });
  });
}
