import { type HttpListener, type HttpListenerOptions, type RpcAuthenticator } from './http-listener.js';
/**
 * High-level AsterServer and AsterClient wrappers.
 *
 * These provide one-liner setup for the common case, hiding the details of
 * endpoint creation, enrollment and transport wiring. Both run on the Rust
 * `AsterServer` / `AsterClient`: an `AsterConfig` whose `[enrollment]`
 * section is set admits only enrolled peers, and each peer's credential
 * roles are what `requires` checks (design:
 * `docs/_internal/working_ideas/aster-enrollment.md` §15).
 */

import { resolve as resolvePath } from 'node:path';

import { ServiceRegistry } from './service.js';
import { LocalTransport } from './transport/local.js';
import { createClient, type AsterClient as ClientProxy } from './client.js';
import type { AsterTransport } from './transport/base.js';
import { AsterConfig, loadIdentity, mergeConfig, toNativeConfig } from './config.js';
import { Node } from './node.js';
import { createLogger, type AsterLogger } from './logging.js';
import type {
  AsterClient as NativeAsterClient,
  DispatchServer as NativeDispatchServer,
  HealthServer,
} from '@aster-rpc/transport';
import { ensureObservability } from './observability.js';
import { DEFAULT_BACKOFF, RpcPattern, RpcScope, type ExponentialBackoff } from './types.js';
import { createXlangCodec, getXlangForyAndType } from './xlang.js';
import { RpcServer } from './dispatch.js';
import type { Interceptor } from './interceptors/base.js';
import { buildTypeGraph, canonicalXlangBytes, contractIdFromContract, fromServiceInfo } from './contract/identity.js';
import type { ContractManifest, ManifestMethod, ManifestField } from './contract/manifest.js';
import { getGeneratedMethodFields, getWireShape, registerGenerated, type RegisterGeneratedOptions } from './generated.js';
import { IrohTransport } from './transport/iroh.js';
import { StatusCode, RpcError } from './status.js';
import { loadNative, tryLoadNative } from './native.js';

// ── Constants ────────────────────────────────────────────────────────────────

/**
 * The `aster.ops.NodeInfo` attribute an `AsterServer` announces its contract
 * registry in (the registry doc's namespace id, 64 hex). Clients read it
 * after connecting to discover services for `proxy()` and generated clients.
 */
export const REGISTRY_NAMESPACE_ATTRIBUTE = 'aster.registry_namespace';

/**
 * A service a server offers, as a client discovers it from the server's
 * contract registry (or as the server lists its own).
 * @group Server and Client
 */
export interface ServiceSummary {
  name: string;
  version: number;
  contractId: string;
  /** `shared` or `session`. */
  pattern: string;
  methods: string[];
  channels?: Record<string, string>;
  /** Serialization modes the server supports for this service (`xlang` = Fory). */
  serializationModes?: string[];
}

// ── AsterServer ──────────────────────────────────────────────────────────────

/** Well-known filename emitted by `npx aster-gen`. */
const GENERATED_FILENAME = 'aster-rpc.generated.ts';

/** The parts of a doc event a sync wait reads. */
interface SyncWaitEvent {
  kind: string;
  syncOk?: boolean;
}

/**
 * Wait up to `timeoutMs` for a completed sync round or for an entry under
 * `prefix`; resolves whether either happened.
 *
 * A failed round (refused, aborted, or an unreachable peer) also ends with
 * `sync_finished`, with `syncOk` false. It counts as no news: waiting
 * continues until the deadline, checking for entries while events are quiet.
 *
 * @internal
 */
export async function waitForInitialSync(
  doc: { queryKeyPrefix(prefix: string): Promise<unknown[]> },
  events: { recv(): Promise<SyncWaitEvent | null> },
  timeoutMs: number,
  prefix = 'contracts/',
): Promise<boolean> {
  const hasEntries = async (): Promise<boolean> => {
    try {
      return (await doc.queryKeyPrefix(prefix)).length > 0;
    } catch {
      return false;
    }
  };
  const deadline = Date.now() + timeoutMs;
  // One receive stays outstanding across quiet intervals, so an event that
  // lands after a timeout is not lost to an abandoned receive.
  let pending: Promise<SyncWaitEvent | null> | null = null;
  const QUIET = Symbol('quiet');
  while (Date.now() < deadline) {
    pending ??= events.recv();
    const waitMs = Math.min(deadline - Date.now(), 1000);
    let timer: ReturnType<typeof setTimeout> | undefined;
    const ev = await Promise.race([
      pending,
      new Promise<typeof QUIET>((r) => { timer = setTimeout(() => r(QUIET), waitMs); }),
    ]).finally(() => clearTimeout(timer));
    if (ev === QUIET) {
      if (await hasEntries()) return true;
      continue;
    }
    pending = null;
    // The subscription ended; nothing more will arrive.
    if (ev === null) return hasEntries();
    if (ev.kind === 'sync_finished' && ev.syncOk) {
      await new Promise((r) => setTimeout(r, 100));
      return true;
    }
  }
  return hasEntries();
}

/**
 * Options for AsterServer.
 * @group Server and Client
 */
export interface AsterServerOptions {
  services: object[];
  /** Explicit native aster.net.Topology baseline opt-in (default false); requires monitoring. */
  builtinTopology?: boolean;
  /** Rust-enforced role policy for every topology baseline method. */
  topologyRequires?: import("./service.js").CapabilityRequirement;
  /**
   * Explicit generated metadata (output of `aster-gen`). When omitted,
   * `start()` auto-imports `./aster-rpc.generated.js` from the working
   * directory. Pass this only when the auto-import can't work (e.g.
   * bundled apps where dynamic import paths aren't resolvable).
   */
  generated?: Pick<RegisterGeneratedOptions, 'SERVICES' | 'WIRE_TYPES' | 'buildAllTypes'>;
  /**
   * Configuration, overlaid on `AsterConfig.fromEnv()` (the `enrollment`
   * section is merged field by field). Set `enrollment` (or the
   * `ASTER_ENROLLMENT_*` variables) to admit only enrolled peers.
   */
  config?: Partial<AsterConfig>;
  /** Path to a `.aster-identity` file whose `[node] secret_key` pins this node's key. Overrides config.identityFile. */
  identity?: string;
  /** Peer name for identity file lookup. */
  peer?: string;
  interceptors?: unknown[];
}

/**
 * High-level Aster RPC server.
 *
 * Starts the Rust `AsterServer` from the resolved {@link AsterConfig},
 * serves the registered services on it, publishes their contracts and prints
 * a startup banner.
 *
 * Without enrollment configured the server is open: any peer may call any
 * method without `requires`. With enrollment configured (`config.enrollment`
 * / `ASTER_ENROLLMENT_*` and the `[trust]` root and credential), only peers
 * holding a credential from the root connect, and each peer's credential
 * roles are what `requires` is checked against, on every call. A
 * misconfiguration fails `start()` with an error naming the setting.
 *
 * @example
 * ```ts
 * // Run `npx aster-gen` first — start() auto-imports aster-rpc.generated.js
 * const server = new AsterServer({
 *   services: [new MissionControl()],
 * });
 * await server.start();
 * console.log(server.address);
 * await server.serve();
 * ```
 *
 * @group Server and Client
 */
export class AsterServer {
  readonly registry: ServiceRegistry;
  readonly config: AsterConfig;
  readonly logger: AsterLogger;
  private _health: HealthServer | null = null;
  private _node: Node | null = null;
  private _rpcServer: RpcServer | null = null;
  private _running = false;
  private _closed = false;
  private _userInterceptors: unknown[] = [];
  private _builtinTopology = false;
  private _topologyRequires?: import("./service.js").CapabilityRequirement;
  private _pendingServices: object[] = [];
  private _explicitGenerated: Pick<RegisterGeneratedOptions, 'SERVICES' | 'WIRE_TYPES'> | undefined;
  private _signalHandlers: (() => void)[] = [];
  private _starting: Promise<void> | undefined;
  private _closing: Promise<void> | undefined;
  private _serviceSummaries: ServiceSummary[] = [];
  private _registryNamespace = '';
  private _servePromise: Promise<void> | null = null;

  constructor(opts: AsterServerOptions) {
    // Rust loaders (ASTER_* env); fields set in code win.
    this.config = mergeConfig(AsterConfig.fromEnv(), opts.config);
    if (opts.identity) {
      this.config.identityFile = opts.identity;
    }
    this.logger = createLogger({
      format: this.config.logFormat,
      level: this.config.logLevel,
      mask: this.config.logMask,
    });
    this.registry = new ServiceRegistry();
    this._userInterceptors = opts.interceptors ?? [];
    this._builtinTopology = opts.builtinTopology ?? false;
    this._topologyRequires = opts.topologyRequires;
    this._pendingServices = opts.services;
    this._explicitGenerated = opts.generated;
  }

  /** Serve the registered services through authenticated HTTPS/WebTransport. */
  async startHttp(options: HttpListenerOptions, authenticate: RpcAuthenticator): Promise<HttpListener> {
    if (this._closed) throw new Error('server is closed');
    await this.start();
    if (this._closed || !this._rpcServer) throw new Error('server is closed');
    return this._rpcServer.startHttp(options, authenticate);
  }

  /**
   * Explicit anonymous HTTPS/WebTransport; no callback is required or inferred.
   *
   * An HTTP caller has no enrollment identity: it never gets enrollment
   * roles, and methods without `requires` stay open to it. Serve
   * enrollment-gated services over Iroh only.
   */
  async startPublicHttp(options: HttpListenerOptions): Promise<HttpListener> {
    if (this._closed) throw new Error('server is closed');
    await this.start();
    if (this._closed || !this._rpcServer) throw new Error('server is closed');
    return this._rpcServer.startPublicHttp(options);
  }

  /**
   * Whether the configuration installed Enrollment: only enrolled peers
   * connect, and `requires` reads each peer's credential roles. `false`
   * before `start()`.
   */
  get enrolled(): boolean {
    return this._native?.enrolled ?? false;
  }

  /**
   * The attributes `requires` sees for `peer` (hex node id) right now: its
   * credential roles (`aster.role`) and claim attributes when enrolled.
   */
  peerAttributes(peer: string): Record<string, string> {
    return { ...(this._native?.peerAttributes(peer) ?? {}) };
  }

  /**
   * Narrow an enrolled peer's roles at runtime. The credential sets the
   * maximum; runtime settings can only restrict. From the peer's next call
   * (calls in flight finish) its effective roles are its credential roles
   * intersected with `roles`: a role the credential lacks is never granted,
   * and `[]` leaves it none. Replaces an earlier restriction, survives
   * renewals of the same credential subject, and ends with the peer's
   * enrollment session (revocation, eviction, expiry). Throws when `peer`
   * (hex node id) has no live enrollment session.
   */
  restrictPeerRoles(peer: string, roles: readonly string[]): void {
    if (!this._native) throw new Error('server is not serving');
    this._native.restrictPeerRoles(peer, [...roles]);
  }

  /**
   * Remove `peer`'s role restriction; its credential roles apply again from
   * its next call. Returns whether one was set.
   */
  clearPeerRestriction(peer: string): boolean {
    if (!this._native) throw new Error('server is not serving');
    return this._native.clearPeerRestriction(peer);
  }

  private get _native(): NativeDispatchServer | null {
    return this._rpcServer?.dispatchServer ?? null;
  }

  /**
   * Start the node and prepare for serving. Idempotent.
   *
   * On first call, auto-imports `aster-rpc.generated.js` from the
   * working directory (unless `generated` was passed explicitly),
   * registers the generated metadata, then registers all services.
   */
  start(): Promise<void> {
    if (this._closed) return Promise.reject(new Error('server is closed'));
    if (this._starting) return this._starting;
    if (this._node) return Promise.resolve();
    this._starting = this._start().finally(() => { this._starting = undefined; });
    return this._starting;
  }

  private async _start(): Promise<void> {
    if (this._node) return;
    if (this._builtinTopology && !this.config.enableMonitoring) {
      throw new Error('builtinTopology requires monitoring');
    }

    // ── Register generated metadata + services ──────────────────────
    // Create the xlang codec early so BUILD_ALL_TYPES (if present in the
    // generated file) can register all @WireType classes before we build
    // the RpcServer. Reuse the same codec instance for RpcServer.
    let xlangCodec: ReturnType<typeof createXlangCodec> | undefined;
    if (this._pendingServices.length > 0) {
      const { fory, Type } = getXlangForyAndType();
      xlangCodec = createXlangCodec(fory, Type);

      if (this._explicitGenerated) {
        // Explicit generated: caller provided the object directly. Pass the
        // codec so Fory types can be registered if buildAllTypes is present.
        registerGenerated({
          ...this._explicitGenerated,
          codec: xlangCodec,
          buildAllTypes: (this._explicitGenerated as any).buildAllTypes,
          fory,
          Type,
        });
      } else {
        try {
          const resolved = await import(
            /* webpackIgnore: true */
            resolvePath(process.cwd(), GENERATED_FILENAME)
          );
          if (resolved.SERVICES && resolved.WIRE_TYPES) {
            registerGenerated({
              SERVICES: resolved.SERVICES,
              WIRE_TYPES: resolved.WIRE_TYPES,
              codec: xlangCodec,
              buildAllTypes: resolved.BUILD_ALL_TYPES,
              fory,
              Type,
            });
          }
        } catch (err: unknown) {
          this.logger.warn(
            `[aster] ${GENERATED_FILENAME} not found in working directory. ` +
            `The runtime will fall back to reflection for type metadata, which ` +
            `cannot handle empty arrays, nullable nested types, or non-default-constructible ` +
            `classes. Run 'npx aster-gen' to generate it.`,
          );
        }
      }
      for (const svc of this._pendingServices) {
        this.registry.register(svc);
      }
      this._pendingServices = [];
    }

    // Load native addon
    const native = loadNative();

    // Initialize contract identity binding (needed for _publishContracts)
    const { setNativeContract } = await import('./contract/identity.js');
    setNativeContract(native);
    const { setNativeTrustConfigPrimitives } = await import('./trust/config-primitives.js');
    setNativeTrustConfigPrimitives(native);

    const config: AsterConfig = {
      ...this.config,
      // The health listener is started below on the node, after the
      // contracts are published (`server.health`).
      healthEnabled: false,
      healthPort: 0,
    };

    // Serve through the Rust `AsterServer` (Rust owns accept, Gate 0,
    // routing, sessions, Gate 3, deadlines). With `[enrollment]` configured
    // it validates the whole section before the node starts, installs
    // Enrollment (which owns Gate 0), serves the enrollment handshake and a
    // configured issuer, and feeds Gate 3 each peer's credential roles.
    // Observability starts from the environment unless the application
    // called `initObservability()` first (design Q2).
    await ensureObservability();
    const rpcServer = new RpcServer({
      registry: this.registry,
      builtinTopology: this._builtinTopology,
      topologyRequires: this._topologyRequires,
      codec: xlangCodec ?? createXlangCodec(),
      interceptors: [...(this._userInterceptors as Interceptor[])],
      logger: this.logger,
      peerAttributes: (peer) => {
        const m = this._native?.peerAttributes(peer);
        return m && Object.keys(m).length ? { ...m } : undefined;
      },
    });
    await rpcServer.startWithConfig(config);
    this._rpcServer = rpcServer;
    const nativeNode = rpcServer.dispatchServer?.node();
    if (!nativeNode) throw new Error('AsterServer: the native server has no node');
    this._node = Node.fromNative(nativeNode);

    const nodeId = this._node.id;
    const rpcAddr = Buffer.from(nodeId).toString('base64');
    this._serviceSummaries = [];
    for (const info of this.registry.getAllServices()) {
      this._serviceSummaries.push({
        name: info.name,
        version: info.version,
        contractId: '',
        pattern: info.scoped ?? 'shared',
        methods: Object.keys(info.methods),
        channels: { rpc: rpcAddr },
        // TS binding speaks XLANG (Fory) by default.
        serializationModes: ['xlang'],
      });
    }

    // Publish contracts to the registry doc (non-fatal on failure) and
    // announce it in `aster.ops.NodeInfo`, where clients discover it.
    await this._publishContracts();
    if (this._registryNamespace) {
      this._native?.setNodeAttribute(REGISTRY_NAMESPACE_ATTRIBUTE, this._registryNamespace);
    }

    await this._startHealth();
    this._running = true;
    this._printBanner();

    // Always log startup info (visible even when stderr is not a TTY)
    const serviceNames = this._serviceSummaries.map(s => s.name).join(', ');
    this.logger.info(
      `server starting runtime=typescript services=[${serviceNames}] mode=${this.enrolled ? 'enrollment' : 'open'}`,
    );
  }

  /**
   * Create a registry doc and publish each service's contract.
   *
   * After publication, `_registryNamespace` is set to the 64-char hex
   * namespace ID, which `start()` announces in `aster.ops.NodeInfo`.
   *
   * Non-fatal: if publication fails, the server still works — consumers
   * just won't get rich contract metadata.
   */
  private async _publishContracts(): Promise<void> {
    try {
      const dc = this._node!.docs();
      const bc = this._node!.blobs();

      // Step 1: Create registry doc and author
      const registryDoc = await dc.create();
      const authorId = await dc.createAuthor();

      // Step 2-10: For each service, build contract and publish
      for (const info of this.registry.getAllServices()) {
        // Build ServiceContract from service info
        const contract = fromServiceInfo(info);
        const contractId = contractIdFromContract(contract);

        // Build manifest with method field descriptors
        const manifest = this._buildManifest(info, contractId);
        const canonicalBytes = canonicalXlangBytes(contract);

        // Collect canonical TypeDef bytes for every wire type reachable
        // from this service's methods. Emitted as `types/{hash}.bin`
        // entries per spec §11.4 so cross-language dynamic clients can
        // walk the canonical type graph without re-running a scanner.
        const typeDefs = this._collectReachableTypeDefs(info);

        // Build collection and upload to blob store
        const { buildCollection: build } =
          await import('./contract/publication.js');
        const entries = build(manifest, canonicalBytes, typeDefs);
        const collectionHash = await bc.addCollection(
          entries.map(([name, data]) => [name, Buffer.from(data)] as [string, Buffer]),
        );
        const ticket = bc.createCollectionTicket(collectionHash);

        // Write ArtifactRef to registry doc
        const { contractKey, manifestKey, versionKey } = await import('./registry/keys.js');
        const artifactRef = {
          contract_id: contractId,
          collection_hash: collectionHash,
          ticket,
          published_by: authorId,
          published_at_epoch_ms: Date.now(),
          collection_format: 'index',
        };
        const encoder = new TextEncoder();
        await registryDoc.setBytes(
          authorId,
          contractKey(contractId),
          Buffer.from(encoder.encode(JSON.stringify(artifactRef))),
        );

        // Write manifest shortcut (avoids blob download round-trip)
        const { manifestToJson } = await import('./contract/manifest.js');
        await registryDoc.setBytes(
          authorId,
          manifestKey(contractId),
          Buffer.from(encoder.encode(manifestToJson(manifest))),
        );

        // Write version pointer
        await registryDoc.setBytes(
          authorId,
          versionKey(info.name, info.version),
          Buffer.from(encoder.encode(contractId)),
        );

        // Update service summary with contract ID
        const summary = this._serviceSummaries.find(s => s.name === info.name);
        if (summary) summary.contractId = contractId;

        this.logger.debug(
          `published contract ${contractId.slice(0, 12)} for ${info.name} v${info.version}`,
        );
      }

      // Share registry doc (read-only) and store namespace ID
      await registryDoc.shareWithAddr('read');
      this._registryNamespace = registryDoc.docId();
      this.logger.info(
        `registry doc ready — namespace=${this._registryNamespace.slice(0, 16)}… ` +
        `services=${this.registry.getAllServices().length}`,
      );

    } catch (err) {
      // Non-fatal: server still works, but cross-language peers will see
      // empty method tables. Log loudly so this isn't silently swallowed --
      // every cross-language interop test depends on this code path.
      const msg = err instanceof Error ? `${err.message}\n${err.stack ?? ''}` : String(err);
      this.logger.error(
        `contract publication failed (non-fatal): cross-language clients ` +
        `will see empty method tables. Cause: ${msg}`,
      );
    }
  }

  /**
   * Walk every wire type reachable from this service's methods
   * (request + response types, transitively through nested `@WireType`
   * fields) and collect their canonical TypeDef bytes keyed by
   * hex-encoded type hash. The scanner (`aster-gen`) stamps both
   * `typeHashHex` and `typeDefBytes` on each WireTypeShape; if either
   * is missing on a type we simply skip it — the publisher falls back
   * to emitting a manifest-only collection, which is still spec-valid
   * but limits cross-language dynamic clients to the flat manifest
   * view.
   */
  private _collectReachableTypeDefs(info: any): Map<string, Uint8Array> {
    const typeDefs = new Map<string, Uint8Array>();
    const roots: (new (...args: any[]) => any)[] = [];
    for (const [, m] of info.methods as Map<string, any>) {
      if (m.requestType) roots.push(m.requestType);
      if (m.responseType) roots.push(m.responseType);
    }
    const reachable = buildTypeGraph(roots);
    // eslint-disable-next-line no-console
    console.error(`[ASTER-DEBUG-SRV] collect ${info.name}: roots=${roots.length} reachable=${reachable.length}`);
    for (const cls of reachable) {
      const shape = getWireShape(cls);
      if (!shape?.typeHashHex || !shape?.typeDefBytes) continue;
      typeDefs.set(shape.typeHashHex, shape.typeDefBytes);
    }
    // eslint-disable-next-line no-console
    console.error(`[ASTER-DEBUG-SRV] collect ${info.name}: ${typeDefs.size} typeDefs`);
    return typeDefs;
  }

  /**
   * Build a ContractManifest from a ServiceInfo with field-level detail.
   */
  private _buildManifest(info: any, contractId: string): ContractManifest {
    const WIRE_TYPE_KEY = Symbol.for('aster.wire_type');
    const methods: ManifestMethod[] = [];
    // Once-per-service-per-method-per-kind dedupe for the fallback
    // warning so server startup doesn't spam logs. Reset each build
    // since _buildManifest runs once per registered service per
    // process.
    const warnedFallback = new Set<string>();

    /**
     * Extract a field list by instantiating a constructor and reading keys.
     * TODO(aster-gen): this is the runtime fallback. The preferred path is
     * `getGeneratedMethodFields(service, version, method)` which reads the
     * list built at scan time by `bunx aster-gen`. Remove this helper once
     * every consumer runs the scanner. Tracked in
     * `ffi_spec/ts-buildtime-audit.md`.
     */
    const extractFields = (Ctor: unknown): ManifestField[] => {
      if (!Ctor || typeof Ctor !== 'function') return [];
      try {
        const inst = new (Ctor as new () => any)();
        const out: ManifestField[] = [];
        for (const key of Object.keys(inst)) {
          const val = inst[key];
          let fieldType = 'str';
          if (typeof val === 'number') fieldType = Number.isInteger(val) ? 'int' : 'float';
          else if (typeof val === 'boolean') fieldType = 'bool';
          else if (Array.isArray(val)) fieldType = 'list';
          else if (val && typeof val === 'object') fieldType = 'dict';
          // Capture the field default so cross-language codegen can emit
          // it. JSON-safe primitives only -- nested objects round-trip via
          // JSON.stringify which would lose Date/Map/Set semantics.
          let defaultValue: unknown = undefined;
          if (
            val === null ||
            typeof val === 'string' ||
            typeof val === 'number' ||
            typeof val === 'boolean'
          ) {
            defaultValue = val;
          } else if (Array.isArray(val)) {
            defaultValue = [];
          } else if (val && typeof val === 'object') {
            defaultValue = {};
          }
          out.push({
            name: key,
            type: fieldType,
            required: val === '' || val === 0 || val === false,
            default: defaultValue,
          });
        }
        return out;
      } catch {
        return [];
      }
    };

    const wireTagOf = (Ctor: unknown): string | undefined => {
      if (!Ctor || typeof Ctor !== 'function') return undefined;
      return (Ctor as any)[WIRE_TYPE_KEY];
    };

    const displayNameOf = (Ctor: unknown): string => {
      if (!Ctor || typeof Ctor !== 'function') return '';
      return (Ctor as any).name ?? '';
    };

    for (const [methodName, mi] of info.methods.entries()) {
      // RpcPattern is a string enum -- 'unary' / 'server_stream' /
      // 'client_stream' / 'bidi_stream'. The previous integer comparison
      // (mi.pattern === 1, etc.) silently fell through to 'unary' for
      // every method, so the published manifest mislabeled @ServerStream
      // and @BidiStream as unary. The server still dispatched correctly
      // from the in-memory MethodInfo, but any client that consumed the
      // manifest -- including the shell and `aster contract gen-client` --
      // would call them as unary and crash on the second response frame.
      const patternStr =
        mi.pattern === RpcPattern.SERVER_STREAM ? 'server_stream' :
        mi.pattern === RpcPattern.CLIENT_STREAM ? 'client_stream' :
        mi.pattern === RpcPattern.BIDI_STREAM ? 'bidi_stream' : 'unary';

      const reqType = mi.requestType;
      const respType = mi.responseType;

      // TypeScript erases parameter types at runtime, so the only way to
      // know what request/response classes a method takes is for the user
      // to pass them explicitly via @Rpc({ request: T, response: U }).
      // When a method is decorated as `@Rpc()` (or any options object that
      // doesn't include `request:` / `response:`), the published manifest
      // has empty fields and broken wire tags -- which silently breaks
      // gen-client for cross-language consumers and breaks the shell's
      // method discovery for native consumers. Warn loudly at server start
      // so the failure mode is visible without making the decorator
      // hard-fail (which would break unit tests of decorator metadata
      // collection that don't actually go through the manifest publish
      // path).
      if (!reqType || !respType) {
        const decoratorByPattern: Record<string, string> = {
          unary: 'Rpc',
          server_stream: 'ServerStream',
          client_stream: 'ClientStream',
          bidi_stream: 'BidiStream',
        };
        const decorator = decoratorByPattern[patternStr] ?? 'Rpc';
        const missing: string[] = [];
        if (!reqType) missing.push('request');
        if (!respType) missing.push('response');
        this.logger.warn(
          `${info.name}.${methodName}: @${decorator} is missing ` +
          `${missing.join(' and ')} type(s). The published manifest will ` +
          `have empty fields, which breaks cross-language gen-client and ` +
          `the shell's method discovery. Pass the constructors explicitly: ` +
          `@${decorator}({ request: SomeRequest, response: SomeResponse })`,
        );
      }

      // Prefer pre-derived manifest fields from the generated file
      // (`bunx aster-gen` output, stashed by `registerGenerated`).
      // Falls back to runtime `extractFields` with a warn-once log so
      // server operators notice when the scanner hasn't been wired in.
      const generatedFields = getGeneratedMethodFields(info.name, info.version, methodName);
      let reqFields: ManifestField[];
      let resFields: ManifestField[];
      if (generatedFields) {
        reqFields = [...generatedFields.requestFields];
        resFields = [...generatedFields.responseFields];
      } else {
        const key = `${info.name}/${info.version}/${methodName}`;
        if (!warnedFallback.has(key)) {
          warnedFallback.add(key);
          this.logger.warn(
            `[aster] ${info.name}.${methodName}: manifest fields built via runtime reflection. ` +
            `Run 'npx aster-gen' to use AST-derived fields — ` +
            `handles empty arrays, nullable nested types, and non-default-constructible classes.`,
          );
        }
        reqFields = extractFields(reqType);
        resFields = extractFields(respType);
      }

      methods.push({
        name: methodName,
        pattern: patternStr,
        // requestType / responseType carry the human-readable display name
        // for the codegen (matches Python manifest layout). The wire tag
        // travels separately so codegen can register the type with Fory.
        requestType: displayNameOf(reqType),
        responseType: displayNameOf(respType),
        requestWireTag: wireTagOf(reqType),
        responseWireTag: wireTagOf(respType),
        timeout: mi.timeout ?? 0,
        idempotent: mi.idempotent ?? false,
        fields: reqFields,
        responseFields: resFields,
      });
    }

    return {
      service: info.name,
      version: info.version,
      contractId,
      canonicalEncoding: 'fory-xlang/0.15',
      typeCount: 0,
      typeHashes: [],
      methodCount: methods.length,
      methods,
      // The TypeScript binding speaks Fory XLANG on the wire.
      serializationModes: ['xlang'],
      scoped: (info.scoped === RpcScope.SESSION || info.scoped === 'stream') ? RpcScope.SESSION : RpcScope.SHARED,
      deprecated: false,
    };
  }

  /**
   * Start accepting connections. Blocks until close() is called.
   */
  async serve(): Promise<void> {
    if (!this._node || !this._rpcServer) {
      throw new Error('AsterServer.serve() called before start()');
    }

    // Install signal handlers for graceful shutdown
    this._installSignalHandlers();

    // Rust serves from start(); this resolves once close() stops it.
    this._servePromise = this._rpcServer.waitUntilStopped();
    try {
      await this._servePromise;
    } catch (e) {
      if (!this._closed) throw e;
    }
  }

  /**
   * Stop accepting connections and close the node.
   */
  close(): Promise<void> {
    if (this._closing) return this._closing;
    this._closed = true;
    this._closing = this._close();
    return this._closing;
  }

  private async _close(): Promise<void> {
    // A lazy start may still be publishing its node/server. Wait for it before taking ownership
    // of cleanup, so a listener request racing close cannot leave a late node behind.
    await this._starting?.catch(() => {});
    this._running = false;

    // Remove signal handlers
    for (const cleanup of this._signalHandlers) cleanup();
    this._signalHandlers = [];

    await this._health?.stop();
    this._health = null;
    // The Rust `AsterServer` owns the node: closing it stops renewal and
    // Enrollment and shuts the node down.
    if (this._rpcServer) {
      await this._rpcServer.close();
    }
    this.logger.info('AsterServer stopped');
  }

  /**
   * The `/healthz` `/readyz` `/metrics` listener (Rust), when the config
   * enables it (`healthPort`, `healthEnabled`; `ASTER_HEALTH_*`). Use it to
   * add readiness checks: `server.health?.check('db').set(true)`.
   */
  get health(): HealthServer | null {
    return this._health;
  }

  private async _startHealth(): Promise<void> {
    const c = this.config;
    if (!(c.healthEnabled ?? (c.healthPort ?? 0) !== 0)) return;
    if (!this._node) return;
    const health = await this._node.startHealth({ host: c.healthHost, port: c.healthPort ?? 0 });
    health.setServices(this._serviceSummaries.length);
    health.setReady(true);
    this._health = health;
  }

  /** The aster1... connection address for clients. */
  get address(): string {
    if (!this._node) throw new Error('Server not started');
    try {
      const native = tryLoadNative();
      if (native) {
        // Carry the direct addresses: a ticket with none is only dialable via
        // relay/discovery, so a relay-less (LAN / loopback) client hangs.
        const info: any = {
          endpointId: this._node.id,
          relayAddr: undefined,
          directAddrs: this._node.directAddresses(),
        };
        return native.asterTicketToString(info);
      }
    } catch { /* fallback */ }
    return this._node.id;
  }

  /** Hex endpoint ID of this server's node. */
  get endpointId(): string {
    if (!this._node) throw new Error('Server not started');
    return this._node.id;
  }

  /** The node this server runs on (blobs, docs, gossip). `close()` the server, not the node. */
  get node(): Node {
    if (!this._node) throw new Error('Server not started');
    return this._node;
  }

  /** Whether the server is running. */
  get running(): boolean { return this._running; }

  /** List of services hosted by this server. */
  get services(): ServiceSummary[] { return [...this._serviceSummaries]; }

  /** Registry doc namespace the server's contracts are published in (after start). */
  get registryNamespace(): string | undefined { return this._registryNamespace || undefined; }

  /** Create a local in-process transport for testing. */
  localTransport(): LocalTransport {
    return new LocalTransport(this.registry);
  }

  // ── Signal handling ─────────────────────────────────────────────────────

  private _installSignalHandlers(): void {
    if (typeof process === 'undefined') return;

    const shutdown = async () => {
      this.logger.info('Server shutting down...');
      await this.close();
    };

    const handler = () => { shutdown(); };
    process.on('SIGTERM', handler);
    process.on('SIGINT', handler);

    this._signalHandlers.push(
      () => { process.off('SIGTERM', handler); },
      () => { process.off('SIGINT', handler); },
    );
  }

  // ── Banner ──────────────────────────────────────────────────────────────

  private _printBanner(): void {
    if (typeof process !== 'undefined' && !process.stderr?.isTTY) return;

    const C = '\x1b[36m';
    const B = '\x1b[1m';
    const D = '\x1b[2m';
    const G = '\x1b[32m';
    const Y = '\x1b[33m';
    const W = '\x1b[37m';
    const R = '\x1b[0m';

    const w = (s: string) => process.stderr.write(s);

    w(`\n${C}${B}`);
    w(`        _    ____ _____ _____ ____\n`);
    w(`       / \\  / ___|_   _| ____|  _ \\\n`);
    w(`      / _ \\ \\___ \\ | | |  _| | |_) |\n`);
    w(`     / ___ \\ ___) || | | |___|  _ <\n`);
    w(`    /_/   \\_\\____/ |_| |_____|_| \\_\\\n`);
    w(`${R}\n`);
    w(`    ${D}RPC after hostnames.${R}\n\n`);

    // Services table
    if (this._serviceSummaries.length > 0) {
      const maxName = Math.max(...this._serviceSummaries.map(s => s.name.length));
      for (const s of this._serviceSummaries) {
        const name = s.name.padEnd(maxName);
        w(`    ${G}●${R} ${B}${name}${R}  ${D}v${s.version}${R}\n`);
      }
      w('\n');
    }

    // Endpoint
    try {
      const nodeId = this._node?.id;
      if (nodeId) {
        const short = nodeId.slice(0, 16) + '…';
        w(`    ${D}node id:${R}   ${W}${short}${R}  ${D}(this node's keypair fingerprint)${R}\n`);
      }
      w(`    ${D}endpoint:${R}  ${this.address}\n`);
    } catch { /* not started yet */ }

    // Mode
    const mode = this.enrolled ? `${G}enrollment${R}` : `${Y}open${R}`;
    w(`    ${D}mode:${R}      ${mode}\n`);

    // Log
    const logFormat = this.config.logFormat || 'text';
    const logLevel = this.config.logLevel || 'info';
    w(`    ${D}log:${R}       ASTER_LOG_FORMAT=${W}${logFormat}${R}  ASTER_LOG_LEVEL=${W}${logLevel}${R}\n`);

    // Runtime
    w(`    ${D}runtime:${R}   aster-rpc (typescript)  iroh 0.97\n`);

    // Copyright
    w(`\n    ${D}Copyright © 2026 Emrul Islam. All rights reserved.${R}\n\n`);
  }
}

// ── AsterClient ──────────────────────────────────────────────────────────────

/**
 * Options for AsterClientWrapper.
 * @group Server and Client
 */
export interface AsterClientOptions {
  /** Connection address (aster1... ticket or hex EndpointId). Falls back to `config.endpointAddr` (`ASTER_ENDPOINT_ADDR`). */
  address?: string;
  /** @deprecated Use `address` instead. */
  endpointAddr?: string;
  transport?: AsterTransport;
  /**
   * Configuration, overlaid on `AsterConfig.fromEnv()` (the `enrollment`
   * section is merged field by field). With enrollment configured the
   * client presents `enrollmentCredentialFile` / `enrollmentCredentialChain`,
   * enrolls with the server and renews its credential.
   */
  config?: Partial<AsterConfig>;
  /** Path to a `.aster-identity` file whose `[node] secret_key` pins this node's key. Overrides config.identityFile. */
  identity?: string;
  /** Peer name for identity file lookup. */
  peer?: string;
  /** Retry configuration for reconnection. */
  retryBackoff?: ExponentialBackoff;
  /** Client interceptors applied to every typed client (`client()` / `service()`). */
  interceptors?: Interceptor[];
  /** Pre-generated type metadata from aster-gen (same as AsterServer.generated). */
  generated?: {
    SERVICES: readonly any[];
    WIRE_TYPES: readonly any[];
    buildAllTypes?: (fory: any, Type: any, codec: any) => Map<string, any>;
  };
}

/**
 * High-level Aster RPC client.
 *
 * Runs on the Rust `AsterClient`: it starts a node from the resolved
 * {@link AsterConfig} and, when enrollment is configured, presents this
 * node's credential, enrolls with the server over the reciprocal handshake,
 * synchronizes the fleet namespace and renews the credential, before the RPC
 * connection opens. Any misconfiguration or refusal rejects `connect()` with
 * an error naming the problem; a node without a credential cannot connect to
 * an enrolled server.
 *
 * @group Server and Client
 */
export class AsterClientWrapper {
  private transport!: AsterTransport;
  private readonly _interceptors: Interceptor[];
  readonly config: AsterConfig;
  private backoff: ExponentialBackoff;
  private _connected = false;
  private _address: string | undefined;
  private _native: NativeAsterClient | null = null;
  private _node: Node | null = null;
  private _services: ServiceSummary[] = [];
  private _servicesLoaded = false;
  private _servicesPromise: Promise<ServiceSummary[]> | null = null;
  private _registryNamespace = '';
  private _discoveryError = '';
  /** `[node] secret_key` (hex) from the `.aster-identity` file. */
  private _identitySecretKey: string | null = null;
  private readonly _generated?: {
    SERVICES: readonly any[];
    WIRE_TYPES: readonly any[];
    buildAllTypes?: (fory: any, Type: any, codec: any) => Map<string, any>;
  };
  // Populated lazily on the first dynamic-proxy / session call. Typed
  // clients don't need the registry doc, so we avoid the join cost at
  // connect() time and pay it only when someone actually calls proxy().
  private _registryDoc: any = null;
  private _registryJoinPromise: Promise<any> | null = null;
  private _manifestCache: Map<string, ContractManifest> = new Map();
  // Decoded canonical TypeDef graph per contractId. Keyed by hex hash
  // and by `{package}/{name}` tag for O(1) lookup from either side.
  // Populated lazily by `ensureTypeDefs` on the first proxy() call for
  // a given contract — the blob collection fetch is then re-used.
  private _typeDefCache: Map<
    string,
    { byTag: Map<string, import('./contract/identity.js').CanonicalTypeDef>;
      byHash: Map<string, import('./contract/identity.js').CanonicalTypeDef> }
  > = new Map();
  // Remote endpoint id (hex) captured during connect() so the dynamic
  // proxy path can join the producer's registry doc via
  // docsClient.joinAndSubscribeNamespace(namespace, peer).
  private _remoteEndpointId = '';
  // The codec picked in connect() — held here so the dynamic proxy /
  // session path can register manifest-derived types against the same
  // Fory instance the underlying IrohTransport is already using.
  private _codec: any = null;
  // Wire-tags already registered with the codec via DynamicTypeFactory,
  // so concurrent proxy() / session() calls don't double-register.
  private _dynamicRegisteredTags: Set<string> = new Set();

  constructor(opts: AsterClientOptions) {
    this.config = mergeConfig(AsterConfig.fromEnv(), opts.config);
    if (opts.identity) {
      this.config.identityFile = opts.identity;
    }
    this._address = opts.address ?? opts.endpointAddr as string | undefined ?? this.config.endpointAddr;
    this.backoff = opts.retryBackoff ?? DEFAULT_BACKOFF;
    this._generated = opts.generated;
    this._interceptors = opts.interceptors ?? [];

    // A `.aster-identity` file pins the node key (`[node] secret_key`).
    const identity = loadIdentity(this.config.identityFile, opts.peer, undefined);
    if (identity?.secretKey) {
      this._identitySecretKey = identity.secretKey;
    }

    if (opts.transport) {
      this.transport = opts.transport;
      this._connected = true;
    }
  }

  /** Whether the client is connected. */
  get connected(): boolean {
    return this._connected;
  }

  /** Whether the client enrolled with the server (enrollment configured). */
  get enrolled(): boolean {
    return this._native?.enrolled ?? false;
  }

  /** This client's node (after `connect()`); closed by `close()`. */
  get node(): Node | null {
    return this._node;
  }

  /** Whether this client renews its enrollment credential. */
  async isRenewing(): Promise<boolean> {
    return (await this._native?.isRenewing()) ?? false;
  }

  /** The enrollment credential chain this client currently presents (it changes when renewal succeeds). */
  async credential(): Promise<string | null> {
    return (await this._native?.credential()) ?? null;
  }

  /**
   * Services the server offers, as discovered by {@link listServices} (empty
   * until then; `connect()` discovers them when the server announces a
   * contract registry).
   */
  get services(): ServiceSummary[] { return [...this._services]; }

  /** Registry namespace ID the server announced (after connect). */
  get registryNamespace(): string | undefined { return this._registryNamespace || undefined; }

  /**
   * Connect to the server and open an RPC transport.
   *
   * If the client was created with a transport, this is a no-op. With
   * enrollment configured, the Rust client enrolls with the server first.
   * Then the server's `aster.ops.NodeInfo` is read for its contract registry
   * and the offered services are discovered.
   */
  async connect(): Promise<void> {
    if (this._connected) return;
    if (!this._address) {
      throw new Error(
        'AsterClient requires an address or transport. ' +
        'Pass address="aster1..." (or set ASTER_ENDPOINT_ADDR) or transport=new IrohTransport(conn).',
      );
    }

    const native = loadNative();

    // Wire the native contract binding for this process. `AsterServer`
    // does this inside `start()`; clients need it too as soon as the
    // dynamic-proxy path starts decoding canonical TypeDef bytes via
    // `decodeTypeDefBytes`. Re-setting is idempotent.
    const { setNativeContract } = await import('./contract/identity.js');
    setNativeContract(native);
    const { setNativeTrustConfigPrimitives } = await import('./trust/config-primitives.js');
    setNativeTrustConfigPrimitives(native);

    await ensureObservability();
    const config: AsterConfig = {
      ...this.config,
      ...(this._identitySecretKey && !this.config.secretKey ? { secretKey: this._identitySecretKey } : {}),
    };
    // The Rust client: node start, enrollment (when configured) and the
    // `aster/1` connection.
    const client = await native.AsterClient.connect(toNativeConfig(config), this._address);
    this._native = client;
    this._node = Node.fromNative(client.node());
    this._remoteEndpointId = client.serverId;

    // Payload codec: Fory XLANG (JSON mode 3 is retired, Aster-SPEC §5.1).
    const codec = createXlangCodec();
    this._codec = codec;

    // Register generated wire types so the client can encode requests
    // without "Failed to detect Fory type" errors.
    if (this._generated?.buildAllTypes) {
      const { fory, Type } = getXlangForyAndType();
      this._generated.buildAllTypes(fory, Type, codec);
    }

    this.transport = new IrohTransport(client.rpcConnection(), codec);
    this._connected = true;

    // Service discovery: the server announces its contract registry in
    // `aster.ops.NodeInfo`. Best-effort: a server without the baseline
    // service or a registry still serves typed clients.
    try {
      const identity = await client.describeServer();
      this._registryNamespace = identity.attributes[REGISTRY_NAMESPACE_ATTRIBUTE] ?? '';
      this._discoveryError = '';
    } catch (e) {
      this._registryNamespace = '';
      this._discoveryError = e instanceof Error ? e.message : String(e);
    }
    if (this._registryNamespace) {
      try {
        await this.listServices();
      } catch {
        /* surfaced again by proxy() */
      }
    }
  }

  /**
   * Discover the services the server offers from its contract registry
   * (cached after the first success).
   */
  async listServices(): Promise<ServiceSummary[]> {
    if (this._servicesLoaded) return [...this._services];
    if (!this._servicesPromise) {
      this._servicesPromise = (async () => {
        const doc = await this.ensureRegistryDoc();
        if (!doc) return [];
        const { manifestFromJson } = await import('./contract/manifest.js');
        const entries = await doc.queryKeyPrefix('manifests/');
        const services: ServiceSummary[] = [];
        for (const entry of entries) {
          try {
            const bytes: Buffer = await doc.readEntryContent(entry.contentHash);
            const m = manifestFromJson(new TextDecoder().decode(bytes));
            services.push({
              name: m.service,
              version: m.version,
              contractId: m.contractId,
              pattern: m.scoped ?? 'shared',
              methods: (m.methods ?? []).map((method: { name: string }) => method.name),
              serializationModes: m.serializationModes,
            });
            this._manifestCache.set(`${m.service}/v${m.version}`, m);
          } catch {
            /* skip a malformed entry */
          }
        }
        this._services = services;
        this._servicesLoaded = true;
        return services;
      })().finally(() => { this._servicesPromise = null; });
    }
    return [...(await this._servicesPromise)];
  }

  /** Create a typed client proxy for a service class. */
  async client<T extends new (...args: any[]) => any>(serviceClass: T): Promise<ClientProxy<InstanceType<T>>> {
    return createClient(serviceClass, this.transport, { interceptors: this._interceptors });
  }

  /** Create a typed client proxy for a service class. */
  service<T extends new (...args: any[]) => any>(serviceClass: T): ClientProxy<InstanceType<T>> {
    return createClient(serviceClass, this.transport, { interceptors: this._interceptors });
  }

  /**
   * Create a dynamic proxy client for a service.
   *
   * On the first call per service, fetches the producer's published
   * manifest from the registry doc, synthesises request/response types
   * via {@link DynamicTypeFactory}, and registers them with the Fory
   * codec so proxy calls speak the same wire as the typed client.
   * Subsequent `proxy(sameName)` calls reuse the cached registration.
   *
   * @example
   * ```ts
   * const mc = await client.proxy("MissionControl");
   * const result = await mc.getStatus({ agentId: "edge-1" });
   * console.log(result.status);
   * ```
   */
  async proxy(serviceName: string): Promise<ProxyClient> {
    const summary = (await this.listServices()).find(s => s.name === serviceName);
    if (!summary) {
      const why = this._discoveryError
        ? `the server could not be described (${this._discoveryError}); an enrolled server ` +
          `refuses nodes without a valid credential`
        : this._registryNamespace
          ? `its contract registry does not list it`
          : `the server announced no contract registry`;
      throw new Error(`proxy('${serviceName}'): service not found: ${why}`);
    }
    const hintTypes = await this._registerDynamicTypesForService(summary);
    if (!hintTypes || hintTypes.size === 0) {
      // Without synthesised Fory types the proxy used to fall back to JSON
      // (mode 3), which is retired. Untyped calls need manifest-driven
      // Fory encode/decode in Rust core (plan D5).
      throw new Error(
        `proxy('${serviceName}'): dynamic Fory pending (plan D5) -- no Fory types could be ` +
        `synthesised for this service from its published contract, and JSON mode is retired. ` +
        `Use a typed client (client.client(ServiceClass)) instead.`,
      );
    }
    if (summary && (summary.pattern === 'session' || summary.pattern === 'stream')) {
      return new SessionProxyClient(
        serviceName, this._node, this.transport, this._codec, hintTypes, summary.version ?? 1,
      ) as any;
    }
    return new ProxyClient(serviceName, this.transport, hintTypes, summary?.version ?? 1);
  }

  /**
   * Join the producer's registry doc lazily on first dynamic call.
   * Typed clients (generated from the contract) don't need it, so we
   * avoid the cost at connect() time.
   */
  private async ensureRegistryDoc(): Promise<any | null> {
    if (this._registryDoc) return this._registryDoc;
    if (!this._registryNamespace || !this._node) return null;
    if (!this._remoteEndpointId) return null;
    if (this._registryJoinPromise) return this._registryJoinPromise;

    this._registryJoinPromise = (async () => {
      try {
        const dc = this._node!.docs();
        const result = await dc.joinAndSubscribeNamespace(
          this._registryNamespace,
          this._remoteEndpointId,
        );
        const doc = result.takeDoc();
        const events = result.takeEvents();

        // Best-effort sync wait. Short total deadline because the producer
        // writes these synchronously on startup so the initial pull is
        // usually quick.
        await waitForInitialSync(doc, events, 6000);
        this._registryDoc = doc;
        return doc;
      } catch (e) {
        this._registryJoinPromise = null;
        throw e;
      }
    })();
    return this._registryJoinPromise;
  }

  /**
   * Fetch and cache a contract manifest for ``(serviceName, version)``.
   * Tries the ``manifests/{contractId}`` shortcut first, falls back to the
   * blob collection via ``fetchContract`` when the shortcut is missing.
   */
  private async ensureManifest(
    serviceName: string,
    version: number,
  ): Promise<ContractManifest | null> {
    const key = `${serviceName}/v${version}`;
    const cached = this._manifestCache.get(key);
    if (cached) return cached;

    const summary = this._services.find(
      s => s.name === serviceName && s.version === version,
    );
    if (!summary || !summary.contractId) return null;

    const doc = await this.ensureRegistryDoc();
    if (!doc) return null;

    const { manifestFromJson } = await import('./contract/manifest.js');
    const { contractKey, manifestKey } = await import('./registry/keys.js');

    let manifest: ContractManifest | null = null;

    // Fast path: inline shortcut.
    try {
      const entries = await doc.queryKeyExact(manifestKey(summary.contractId));
      if (entries.length) {
        const contentHash = entries[0].contentHash;
        const bytes: Buffer = await doc.readEntryContent(contentHash);
        manifest = manifestFromJson(new TextDecoder().decode(bytes));
      }
    } catch {
      /* fall through to slow path */
    }

    // Slow path: read ArtifactRef → fetch collection from blobs.
    if (!manifest) {
      try {
        const entries = await doc.queryKeyExact(contractKey(summary.contractId));
        if (entries.length) {
          const contentHash = entries[0].contentHash;
          const bytes: Buffer = await doc.readEntryContent(contentHash);
          const artifactRef = JSON.parse(new TextDecoder().decode(bytes));
          const collectionHash = artifactRef.collection_hash;
          if (collectionHash) {
            const { fetchContract } = await import('./contract/publication.js');
            const blobs = this._node!.blobs();
            manifest = await fetchContract(blobs, collectionHash);
          }
        }
      } catch {
        /* surface as "no manifest available" */
      }
    }

    if (manifest) this._manifestCache.set(key, manifest);
    return manifest;
  }

  /**
   * Fetch + decode the canonical TypeDef graph for ``contractId``.
   * Returns two lookup views (by tag and by hex hash) sharing the same
   * underlying CanonicalTypeDef objects. Cached per contract.
   *
   * When the publisher didn't emit `types/{hash}.bin` blobs (legacy TS
   * pre-§11.4 parity, or third-party bindings that don't yet match),
   * the resulting maps are empty — callers then fall back to the flat
   * manifest-field proxy path.
   */
  private async ensureTypeDefs(
    contractId: string,
  ): Promise<{
    byTag: Map<string, import('./contract/identity.js').CanonicalTypeDef>;
    byHash: Map<string, import('./contract/identity.js').CanonicalTypeDef>;
  }> {
    const cached = this._typeDefCache.get(contractId);
    if (cached) return cached;

    const empty = {
      byTag: new Map<string, import('./contract/identity.js').CanonicalTypeDef>(),
      byHash: new Map<string, import('./contract/identity.js').CanonicalTypeDef>(),
    };

    const doc = await this.ensureRegistryDoc();
    if (!doc) {
      // eslint-disable-next-line no-console
      console.error(`[ASTER-DEBUG] ensureTypeDefs: ensureRegistryDoc returned null`);
      return empty;
    }

    try {
      const { contractKey } = await import('./registry/keys.js');
      const entries = await doc.queryKeyExact(contractKey(contractId));
      if (!entries.length) return empty;
      const contentHash = entries[0].contentHash;
      const bytes: Buffer = await doc.readEntryContent(contentHash);
      const artifactRef = JSON.parse(new TextDecoder().decode(bytes));
      const collectionHash = artifactRef.collection_hash;
      if (!collectionHash) return empty;

      const blobs = this._node!.blobs();

      // The contract collection was produced on the remote peer; its
      // HashSeq + child blobs aren't yet in our local store. Pull the
      // whole collection via the ticket stored alongside the ArtifactRef
      // before listing/reading. This mirrors the `fetchContract` flow a
      // manifest-only consumer would take on the slow path.
      // The contract collection was authored on the remote peer, so we
      // need to pull the HashSeq + all child blobs into the local store
      // before listing / reading them. downloadCollectionHash sets
      // BlobFormat::HashSeq explicitly so child blobs come down in the
      // same round trip (ticket-based downloadCollection misses this and
      // fails with "hash not found" on read). Mirrors Python's flow at
      // runtime.py:1841.
      if (this._remoteEndpointId) {
        try {
          await blobs.downloadCollectionHash(collectionHash, this._remoteEndpointId);
        } catch {
          // Silently retry via direct reads below — covers the case
          // where the collection is already present locally (e.g. same-
          // process in-memory tests).
        }
      }

      const { fetchContractTypeDefBytes } = await import('./contract/publication.js');
      const { decodeTypeDefBytes } = await import('./contract/identity.js');
      const rawByHash = await fetchContractTypeDefBytes(blobs, collectionHash);

      const byTag = new Map<string, import('./contract/identity.js').CanonicalTypeDef>();
      const byHash = new Map<string, import('./contract/identity.js').CanonicalTypeDef>();
      for (const [hashHex, rawBytes] of rawByHash) {
        try {
          const td = decodeTypeDefBytes(rawBytes);
          byHash.set(hashHex, td);
          byTag.set(`${td.package}/${td.name}`, td);
        } catch {
          // Skip individual malformed blobs so a single bad entry
          // doesn't block the rest of the graph.
        }
      }
      const out = { byTag, byHash };
      this._typeDefCache.set(contractId, out);
      return out;
    } catch {
      return empty;
    }
  }

  /**
   * Register request/response types for a service against the live Fory
   * codec, so dynamic proxy / session calls encode as Fory XLANG instead
   * of failing with "Failed to detect Fory type" on a plain object.
   * Returns `undefined` when the service does not advertise xlang or no
   * types could be synthesised (the caller then refuses: plan D5).
   */
  private async _registerDynamicTypesForService(
    summary: ServiceSummary | undefined,
  ): Promise<Map<string, new (init?: any) => any> | undefined> {
    if (!summary) return undefined;
    if (!this._codec || typeof this._codec.registerType !== 'function') return undefined;
    const modes = summary.serializationModes;
    if (modes && modes.length > 0 && !modes.includes('xlang')) return undefined;

    const manifest = await this.ensureManifest(summary.name, summary.version);
    if (!manifest || !manifest.methods?.length) return undefined;

    const methods = manifest.methods as unknown as ManifestMethod[];

    try {
      const { DynamicTypeFactory } = await import('./dynamic.js');
      const { fory, Type } = getXlangForyAndType();
      const factory = new DynamicTypeFactory();

      // Only run the Fory registration the first time we see each wire
      // tag — factory.registerWithFory is idempotent but the codec
      // dedupes by typename, so doing it once per tag is cheaper.
      const needsRegister = methods.some(m =>
        (m.requestWireTag && !this._dynamicRegisteredTags.has(m.requestWireTag))
        || (m.responseWireTag && !this._dynamicRegisteredTags.has(m.responseWireTag)),
      );
      if (needsRegister) {
        // Hybrid path: prefer canonical TypeDef registration when the
        // publisher emitted per-type blobs — that gives us the full
        // type graph including nested refs, which the flat manifest
        // alone can't express. Fall back to manifest-field registration
        // for any root wire tag the TypeDef graph didn't resolve (e.g.
        // legacy publishers or methods without inferable wire tags).
        const typeDefs = summary.contractId
          ? await this.ensureTypeDefs(summary.contractId)
          : { byTag: new Map(), byHash: new Map() };

        let resolved = new Set<string>();
        if (typeDefs.byTag.size > 0) {
          const roots: string[] = [];
          for (const m of methods) {
            if (m.requestWireTag) roots.push(m.requestWireTag);
            if (m.responseWireTag) roots.push(m.responseWireTag);
          }
          resolved = factory.registerFromTypeDefs(roots, typeDefs, Type, this._codec);
        }

        // Flat-manifest fallback for any method whose root types
        // weren't covered by the TypeDef graph. Keeps backward compat
        // with legacy publishers and handles @ClientStream methods
        // without inferable request types (tracked in aster-backlog.md).
        const manifestFallback = methods.filter(m => {
          const reqOk = !m.requestWireTag || resolved.has(m.requestWireTag);
          const resOk = !m.responseWireTag || resolved.has(m.responseWireTag);
          return !(reqOk && resOk);
        });
        if (manifestFallback.length > 0) {
          factory.registerWithFory(manifestFallback, fory, Type, this._codec);
        }
        for (const m of methods) {
          if (m.requestWireTag) this._dynamicRegisteredTags.add(m.requestWireTag);
          if (m.responseWireTag) this._dynamicRegisteredTags.add(m.responseWireTag);
        }
      } else {
        // Need class handles even on the cached path; populate factory
        // types without re-registering with Fory.
        for (const m of methods) {
          if (m.requestWireTag) factory.synthesizeForMethod(m);
          if (m.responseWireTag) factory.synthesizeForMethod(m);
        }
      }

      // Build {methodName → request ctor} so the ProxyClient can thread
      // the class as Fory `hintType` at call time. Without this, Fory's
      // encode(plainObject) fails with "Failed to detect the Fory type".
      const hintTypes = new Map<string, new (init?: any) => any>();
      for (const m of methods) {
        if (!m.requestWireTag) continue;
        const cls = factory.get(m.requestWireTag);
        if (cls) hintTypes.set(m.name, cls as unknown as new (init?: any) => any);
      }
      return hintTypes;
    } catch {
      // Swallow and let the call fail at encode time with a clearer
      // error — better than a partially-registered codec state.
      return undefined;
    }
  }

  /** Reconnect with exponential backoff. */
  async reconnect(
    connectFn: () => Promise<AsterTransport>,
    maxAttempts = 5,
  ): Promise<void> {
    let delay = this.backoff.initialMs;
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      try {
        this.transport = await connectFn();
        this._connected = true;
        return;
      } catch (e) {
        if (attempt === maxAttempts) throw e;
        const jitter = delay * this.backoff.jitter * (Math.random() * 2 - 1);
        const waitMs = Math.min(delay + jitter, this.backoff.maxMs);
        await new Promise(r => setTimeout(r, waitMs));
        delay = Math.min(delay * this.backoff.multiplier, this.backoff.maxMs);
      }
    }
  }

  /** Close the client, its transport and its node (stops credential renewal). */
  async close(): Promise<void> {
    this._connected = false;
    if (this.transport) await this.transport.close();
    this._node = null;
    if (this._native) {
      const client = this._native;
      this._native = null;
      try { await client.close(); } catch { /* ignore */ }
    }
  }
}

// ── ProxyClient ──────────────────────────────────────────────────────────────

/**
 * Dynamic proxy client -- invokes RPC methods without local type definitions.
 *
 * Created via `AsterClientWrapper.proxy("ServiceName")`. Supports all four
 * RPC patterns:
 *
 * ```ts
 * const mc = client.proxy("MissionControl");
 *
 * // Unary
 * const status = await mc.getStatus({ agent_id: "edge-7" });
 *
 * // Client streaming — pass an async iterable
 * const result = await mc.ingestMetrics(asyncGenerator());
 *
 * // Server streaming — use .stream()
 * for await (const entry of mc.tailLogs.stream({ level: "info" })) { ... }
 *
 * // Bidi streaming — use .bidi()
 * const ch = mc.runCommand.bidi();
 * await ch.open();
 * await ch.send({ command: "ls" });
 * for await (const r of ch) { ... }
 * ```
 *
 * @group Server and Client
 */
export class ProxyClient {
  constructor(
    private readonly serviceName: string,
    private readonly transport: AsterTransport,
    // Optional: per-method request class used as Fory `hintType`. When
    // present, a plain-dict payload is wrapped into a typed instance
    // inside the codec so Fory can detect the wire tag. Populated by
    // `AsterClientWrapper.proxy()` from manifest-synthesised types.
    private readonly _hintTypes?: Map<string, new (init?: any) => any>,
    private readonly _version: number = 1,
  ) {
    return new Proxy(this, {
      get(target, prop: string) {
        if (prop in target || typeof prop === 'symbol') {
          return (target as any)[prop];
        }
        // `await client.proxy(name)` returns a Promise<ProxyClient>; when it
        // resolves, the Promise machinery sniffs `.then` on the result to
        // check if it's a thenable and chain. Our Proxy's default behaviour
        // synthesises a fn for any string prop — so `.then` would look like
        // a fn, the runtime would call `.then(resolve, reject)`, and our
        // _proxyMethod would treat `resolve` as the RPC payload. Explicitly
        // short-circuit Promise-chain property names to `undefined` so the
        // resolved value is treated as a plain object.
        if (prop === 'then' || prop === 'catch' || prop === 'finally') {
          return undefined;
        }
        const hintType = target._hintTypes?.get(prop);
        return _proxyMethod(target.serviceName, prop, target.transport, hintType, target._version);
      },
    });
  }
}

/** A bound proxy method supporting all RPC patterns. */
function _proxyMethod(
  serviceName: string,
  methodName: string,
  transport: AsterTransport,
  hintType?: new (init?: any) => any,
  version = 1,
) {
  // The callable: detects async iterables for client streaming, else unary
  const fn = async (payload?: unknown) => {
    // Detect client streaming: payload is an async iterable (but not a plain object)
    if (payload != null && typeof payload === 'object' && Symbol.asyncIterator in payload) {
      const wrapped = hintType
        ? _wrapAsyncIterable(payload as AsyncIterable<unknown>, hintType)
        : (payload as AsyncIterable<unknown>);
      return transport.clientStream(serviceName, methodName, wrapped, { version });
    }
    // Default: unary. If the user accidentally calls a server-streaming
    // method via `await proxy.method(...)`, the underlying transport will
    // see a second response frame and throw with "multiple response
    // frames". Catch that and re-raise with an actionable hint pointing
    // at `proxy.method.stream(...)` / `.bidi()`.
    try {
      return await transport.unary(serviceName, methodName, payload ?? {}, { hintType, version });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      if (msg.includes('multiple response frames')) {
        throw new Error(
          `'${serviceName}.${methodName}' is a streaming RPC and cannot be ` +
          `called as a unary 'await proxy.${methodName}(...)'.\n` +
          `  - For server-streaming methods, iterate the result of ` +
          `'proxy.${methodName}.stream(...)':\n` +
          `      for await (const item of proxy.${methodName}.stream({...})) { ... }\n` +
          `  - For bidi-streaming methods, use 'proxy.${methodName}.bidi()'.`,
        );
      }
      throw err;
    }
  };

  // .stream() — server streaming
  fn.stream = (payload?: unknown): AsyncIterable<unknown> => {
    return transport.serverStream(serviceName, methodName, payload ?? {}, { hintType, version });
  };

  // .bidi() — bidirectional streaming. Returns a lazy wrapper so callers
  // can do `const ch = m.bidi(); await ch.open(); await ch.send(...)` —
  // mirrors Python's _ProxyBidiChannel behaviour. Opening eagerly here
  // would force every call site to handle the connect failure synchronously.
  fn.bidi = (): ProxyBidiChannel => {
    return new ProxyBidiChannel(serviceName, methodName, transport, hintType, version);
  };

  return fn;
}

async function* _wrapAsyncIterable(
  source: AsyncIterable<unknown>,
  ctor: new (init?: any) => any,
): AsyncIterable<unknown> {
  for await (const item of source) {
    if (item && typeof item === 'object' && !Array.isArray(item) && !(item instanceof ctor)) {
      yield new ctor(item);
    } else {
      yield item;
    }
  }
}

/**
 * Lazy bidi-stream wrapper used by ProxyClient. Opens the underlying
 * transport channel on first .open() / .send() / iteration.
 */
export class ProxyBidiChannel {
  private _channel: import('./transport/base.js').BidiChannel | null = null;

  constructor(
    private readonly serviceName: string,
    private readonly methodName: string,
    private readonly transport: AsterTransport,
    private readonly _hintType?: new (init?: any) => any,
    private readonly _version: number = 1,
  ) {}

  async open(): Promise<void> {
    if (this._channel) return;
    this._channel = this.transport.bidiStream(this.serviceName, this.methodName, { version: this._version });
  }

  async send(payload: unknown): Promise<void> {
    if (!this._channel) await this.open();
    const wired = this._hintType
      && payload && typeof payload === 'object' && !Array.isArray(payload)
      && !(payload instanceof this._hintType)
      ? new this._hintType(payload)
      : payload;
    await this._channel!.send(wired);
  }

  async close(): Promise<void> {
    if (this._channel) await this._channel.close();
  }

  [Symbol.asyncIterator](): AsyncIterator<unknown> {
    if (!this._channel) {
      // Open on the fly so `for await` after `await ch.send()` (which
      // already opened) and `for await` without an explicit open both work.
      this._channel = this.transport.bidiStream(this.serviceName, this.methodName, { version: this._version });
    }
    return this._channel[Symbol.asyncIterator]();
  }
}

// ── SessionProxyClient ──────────────────────────────────────────────────────

/**
 * Dynamic proxy client for session-scoped services (new multiplexed-streams
 * protocol, spec §6 / §7.5).
 *
 * Allocates a stable `sessionId` on construction and routes every proxy
 * method call through an `IrohTransport` wired with that id. The transport
 * writes each call's `StreamHeader` with `sessionId=N` and `method=<name>`
 * so the server lands every call on the same per-connection session
 * instance. The pre-migration implementation sent `StreamHeader(method="")`
 * plus `FLAG_CALL`-wrapped CallHeaders on a single bidi stream — that
 * protocol is dead and cross-binding matrices against any new-protocol
 * server fail with `UNIMPLEMENTED: Method '<service>.'`.
 *
 * Each proxy instance uses a fresh sessionId (not shared across proxies)
 * so that creating two proxies in the same process yields two independent
 * server-side instances — matching the old behaviour.
 */
class SessionProxyClient {
  private readonly _sessionTransport: AsterTransport;
  private _closed = false;
  private readonly _hintTypes?: Map<string, new (init?: any) => any>;

  constructor(
    private readonly serviceName: string,
    _node: any,
    transport: AsterTransport,
    codec?: any,
    hintTypes?: Map<string, new (init?: any) => any>,
    private readonly _version: number = 1,
  ) {
    this._hintTypes = hintTypes;
    // A transport on the same connection pinned to a fresh session id from
    // the connection's shared counter (core), so ids never collide with
    // other clients on it; every call carries `StreamHeader.sessionId=N`.
    if (!(transport instanceof IrohTransport)) {
      throw new RpcError(
        StatusCode.UNAVAILABLE,
        'SessionProxyClient: needs an IrohTransport',
      );
    }
    const activeCodec = codec ?? transport.codec ?? createXlangCodec();
    this._sessionTransport = new IrohTransport(transport.conn, activeCodec, {
      sessionId: transport.native.nextSessionId(),
      rpc: transport.native,
    });

    return new Proxy(this, {
      get(target, prop: string) {
        if (prop in target || typeof prop === 'symbol') {
          return (target as any)[prop];
        }
        // Same thenable guard as ProxyClient — see the comment there.
        if (prop === 'then' || prop === 'catch' || prop === 'finally') {
          return undefined;
        }
        return target._sessionMethod(prop);
      },
    });
  }

  private _sessionMethod(methodName: string) {
    const self = this;
    const hintType = self._hintTypes?.get(methodName);
    const fn = async (payload?: unknown): Promise<unknown> => {
      return self._callUnary(methodName, payload ?? {}, hintType);
    };
    fn.stream = (payload?: unknown): AsyncIterable<unknown> => {
      return self._sessionTransport.serverStream(
        self.serviceName,
        methodName,
        payload ?? {},
        { hintType, version: self._version },
      );
    };
    fn.bidi = (): ProxyBidiChannel => {
      return new ProxyBidiChannel(self.serviceName, methodName, self._sessionTransport, hintType, self._version);
    };
    return fn;
  }

  private async _callUnary(
    method: string,
    request: unknown,
    hintType?: new (init?: any) => any,
  ): Promise<unknown> {
    if (this._closed) {
      throw new RpcError(StatusCode.FAILED_PRECONDITION, 'SessionProxyClient is closed');
    }
    return await this._sessionTransport.unary(this.serviceName, method, request, { hintType, version: this._version });
  }

  /**
   * Close the proxy. Marks the proxy closed so subsequent method
   * calls fail fast. Does NOT close the underlying QUIC connection
   * even though the session transport has a `.close()` method --
   * `IrohTransport.close()` tears down the whole connection (shared
   * with other proxies / sessions), so calling it from here would
   * break siblings. The connection is owned by the parent
   * `AsterClientWrapper` and closed when it does.
   *
   * Must be a real method (not synthesised by the JS Proxy) so that
   * `proxy.close()` calls this implementation instead of routing
   * "close" through the session protocol as a remote method name.
   */
  async close(): Promise<void> {
    this._closed = true;
  }
}
