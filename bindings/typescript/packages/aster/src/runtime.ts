/**
 * High-level AsterServer and AsterClient wrappers.
 *
 * These provide one-liner setup for the common case, hiding the
 * details of endpoint creation, admission, and transport wiring.
 */

import { ServiceRegistry } from './service.js';
import { LocalTransport } from './transport/local.js';
import { createClient, type AsterClient as ClientProxy } from './client.js';
import type { AsterTransport } from './transport/base.js';
import type { AsterConfig } from './config.js';
import { configFromEnv } from './config.js';
import { createLogger, type AsterLogger } from './logging.js';
import { HealthServer } from './health.js';
import { DEFAULT_BACKOFF, RpcPattern, RpcScope, type ExponentialBackoff } from './types.js';
import { JsonCodec } from './codec.js';
import { createXlangCodec, getXlangForyAndType } from './xlang.js';
import { RpcServer } from './server.js';

import { handleConsumerAdmissionConnection, performAdmission, type ConsumerAdmissionOpts, type ServiceSummary } from './trust/consumer.js';
import type { ConsumerEnrollmentCredential } from './trust/credentials.js';
import { loadIdentity, parseSimpleToml } from './config.js';
import { handleDelegatedAdmissionConnection, type DelegatedAdmissionPolicy } from './trust/delegated.js';
import { MeshEndpointHook } from './trust/hooks.js';
import { PeerAttributeStore } from './peer-store.js';
import { CapabilityInterceptor } from './interceptors/capability.js';
import type { Interceptor } from './interceptors/base.js';
import { canonicalXlangBytes, contractIdFromContract, fromServiceInfo } from './contract/identity.js';
import type { ContractManifest, ManifestMethod, ManifestField } from './contract/manifest.js';
import { getGeneratedMethodFields, registerGenerated, type RegisterGeneratedOptions } from './generated.js';
import { IrohTransport } from './transport/iroh.js';
import { StatusCode, RpcError } from './status.js';

// ── Constants ────────────────────────────────────────────────────────────────

const ALPN_CONSUMER_ADMISSION = 'aster.consumer_admission';
const ALPN_PRODUCER_ADMISSION = 'aster.producer_admission';
const ALPN_DELEGATED_ADMISSION = 'aster.admission';
const RPC_ALPN = 'aster/1';

// ── Errors ───────────────────────────────────────────────────────────────────

/**
 * Raised when a consumer is refused by the server's admission check.
 *
 * The server never reveals *why* admission failed (no oracle leak), so this
 * error enumerates the common causes as a hint to the user rather than a
 * precise diagnosis. Its `message` is a multi-line actionable hint suitable
 * for direct CLI output.
 *
 * @group Server and Client
 */
export class AdmissionDeniedError extends Error {
  readonly hadCredential: boolean;
  readonly credentialFile: string | null;
  readonly ourEndpointId: string;
  readonly serverAddress: string;

  constructor(opts: {
    hadCredential: boolean;
    credentialFile: string | null;
    ourEndpointId: string;
    serverAddress: string;
  }) {
    const shortId = opts.ourEndpointId
      ? opts.ourEndpointId.slice(0, 16) + '...'
      : '<unknown>';
    let message: string;
    if (!opts.hadCredential) {
      message =
        'consumer admission denied -- this server requires a credential.\n' +
        '  - Get an enrollment credential file (.cred) from the server\'s operator.\n' +
        '  - Then retry with: --rcan <path/to/file.cred>\n' +
        '    (or set ASTER_ENROLLMENT_CREDENTIAL=<path> in the environment)';
    } else {
      const credLabel = opts.credentialFile ?? '<credential>';
      message =
        `consumer admission denied -- the server rejected your credential.\n` +
        `  credential: ${credLabel}\n` +
        `  your node:  ${shortId}\n` +
        '  Common causes:\n' +
        '    1. The credential expired (check the \'Expires\' field on the file).\n' +
        '    2. The credential was issued to a DIFFERENT node. Credentials are\n' +
        '       bound to a single endpoint id: if you copied this file from\n' +
        '       another machine/process, the server sees a different node id\n' +
        '       and refuses admission. Ask the operator to re-issue it for\n' +
        `       endpointId=${shortId}.\n` +
        '    3. The server trusts a different root key than the one that signed\n' +
        '       this credential.\n' +
        '    4. The credential\'s role/capabilities don\'t match this server\'s\n' +
        '       policy (the server may reject unknown capabilities outright).';
    }
    super(message);
    this.name = 'AdmissionDeniedError';
    this.hadCredential = opts.hadCredential;
    this.credentialFile = opts.credentialFile;
    this.ourEndpointId = opts.ourEndpointId;
    this.serverAddress = opts.serverAddress;
  }
}

// ── AsterServer ──────────────────────────────────────────────────────────────

/**
 * Options for AsterServer.
 * @group Server and Client
 */
/** Well-known filename emitted by `npx aster-gen`. */
const GENERATED_FILENAME = 'aster-rpc.generated.ts';

export interface AsterServerOptions {
  services: object[];
  /**
   * Explicit generated metadata (output of `aster-gen`). When omitted,
   * `start()` auto-imports `./aster-rpc.generated.js` from the working
   * directory. Pass this only when the auto-import can't work (e.g.
   * bundled apps where dynamic import paths aren't resolvable).
   */
  generated?: Pick<RegisterGeneratedOptions, 'SERVICES' | 'WIRE_TYPES' | 'buildAllTypes'>;
  config?: Partial<AsterConfig>;
  /** Path to .aster-identity file. Overrides config.identityFile. */
  identity?: string;
  /** Peer name for identity file lookup. */
  peer?: string;
  /** Allow all consumers without credentials (dev mode). Default: true. */
  allowAllConsumers?: boolean;
  interceptors?: unknown[];
}

/**
 * High-level Aster RPC server.
 *
 * Creates an IrohNode, serves RPC over QUIC, handles consumer admission,
 * and prints a startup banner.
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
  private health: HealthServer;
  private _node: any = null;
  private _rpcServer: RpcServer | null = null;
  private _hook: MeshEndpointHook;
  private _peerStore: PeerAttributeStore;
  private _delegationPolicies: Map<string, DelegatedAdmissionPolicy> = new Map();
  private _running = false;
  private _closed = false;
  private _allowAllConsumers: boolean;
  private _userInterceptors: unknown[] = [];
  private _pendingServices: object[] = [];
  private _explicitGenerated: Pick<RegisterGeneratedOptions, 'SERVICES' | 'WIRE_TYPES'> | undefined;
  private _signalHandlers: (() => void)[] = [];
  private _serviceSummaries: ServiceSummary[] = [];
  private _registryNamespace = '';
  private _servePromise: Promise<void> | null = null;
  private _admissionAbort: AbortController | null = null;

  constructor(opts: AsterServerOptions) {
    this.config = { ...configFromEnv(), ...opts.config } as AsterConfig;
    if (opts.identity) {
      this.config.identityFile = opts.identity;
    }
    this.logger = createLogger({
      format: this.config.logFormat,
      level: this.config.logLevel,
      mask: this.config.logMask,
    });
    this.registry = new ServiceRegistry();
    this.health = new HealthServer({
      port: this.config.healthPort,
      host: this.config.healthHost,
    });
    this._peerStore = new PeerAttributeStore();
    this._allowAllConsumers = opts.allowAllConsumers ?? true;
    // In dev mode the hook must allow unenrolled peers, otherwise post-admission
    // RPC connections would be denied (they reach Gate 0 before the peer-store
    // entry from admission is checked, and the peer wouldn't be there for
    // ephemeral consumers).
    this._hook = new MeshEndpointHook(this._allowAllConsumers, this._peerStore);
    this._userInterceptors = opts.interceptors ?? [];
    this._pendingServices = opts.services;
    this._explicitGenerated = opts.generated;
  }

  /**
   * Create the IrohNode and prepare for serving. Idempotent.
   *
   * On first call, auto-imports `aster-rpc.generated.js` from the
   * working directory (unless `generated` was passed explicitly),
   * registers the generated metadata, then registers all services.
   */
  async start(): Promise<void> {
    if (this._node) return;

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
            require('node:path').resolve(process.cwd(), GENERATED_FILENAME)
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
    const native = await loadNative();
    if (!native) {
      throw new Error(
        'Aster native addon not found. Build with: cd native && npx napi build --release --platform',
      );
    }

    // Initialize contract identity binding (needed for _publishContracts)
    const { setNativeContract } = await import('./contract/identity.js');
    setNativeContract(native);

    // Create node with RPC + admission ALPNs.
    //
    // Gate 0 hooks must be enabled whenever any admission gate is active
    // (allow_all_consumers=false). Without enable_hooks=true, the
    // before_connect callbacks never fire and the admitted-set is
    // unenforced — every connection is allowed regardless of admission.
    const alpns = [
      Buffer.from(RPC_ALPN),
      Buffer.from(ALPN_CONSUMER_ADMISSION),
      Buffer.from(ALPN_PRODUCER_ADMISSION),
      Buffer.from(ALPN_DELEGATED_ADMISSION),
    ];
    const gate0Needed = !this._allowAllConsumers;
    const endpointConfig = gate0Needed
      ? { enableHooks: true, hookTimeoutMs: 5000 }
      : undefined;
    this._node = await native.IrohNode.memoryWithAlpns(alpns, endpointConfig);

    // Build service summaries for admission response
    // Encode the server's own node ID as the RPC channel address
    const nodeId = this._node.nodeId();
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
        // Cross-language consumers see this and pick the matching codec.
        serializationModes: ['xlang'],
      });
    }

    // Auto-wire CapabilityInterceptor if any service declares requires=
    const interceptors: Interceptor[] = [...(this._userInterceptors as Interceptor[])];
    let anyHasRequires = false;
    for (const info of this.registry.getAllServices()) {
      if (info.requires) anyHasRequires = true;
      for (const mi of info.methods.values()) {
        if (mi.requires) anyHasRequires = true;
      }
    }
    const hasCapInterceptor = interceptors.some(i => i instanceof CapabilityInterceptor);
    if ((!this._allowAllConsumers || anyHasRequires) && !hasCapInterceptor) {
      const cap = new CapabilityInterceptor();
      // Register requirements from service/method declarations
      for (const info of this.registry.getAllServices()) {
        for (const [methodName, mi] of info.methods.entries()) {
          const req = mi.requires ?? info.requires;
          if (req) cap.setRequirement(info.name, methodName, req);
        }
      }
      interceptors.unshift(cap);
    }

    // Create the RPC server with Fory XLANG codec.
    this._rpcServer = new RpcServer({
      registry: this.registry,
      codec: xlangCodec!,
      interceptors,
      logger: this.logger,
      peerStore: this._peerStore,
    });

    // Publish contracts to registry doc (non-fatal on failure)
    await this._publishContracts();

    await this.health.start();
    this._running = true;
    this._printBanner();

    // Always log startup info (visible even when stderr is not a TTY)
    const serviceNames = this._serviceSummaries.map(s => s.name).join(', ');
    this.logger.info(
      `server starting runtime=typescript services=[${serviceNames}] mode=${this._allowAllConsumers ? 'open-gate' : 'trusted'}`,
    );
  }

  /**
   * Create a registry doc and publish each service's contract.
   *
   * After publication, `_registryNamespace` is set to the 64-char hex
   * namespace ID so the admission response can return it.
   *
   * Non-fatal: if publication fails, the server still works — consumers
   * just won't get rich contract metadata.
   */
  private async _publishContracts(): Promise<void> {
    try {
      const dc = this._node.docsClient();
      const bc = this._node.blobsClient();

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

        // Build collection and upload to blob store
        const { buildCollection: build } =
          await import('./contract/publication.js');
        const entries = build(manifest, canonicalBytes);
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
      // The TypeScript binding speaks XLANG on the wire.
      // Cross-language consumers reading the manifest use SerializationMode.JSON.
      serializationModes: ['json'],
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

    // Enable RPC server connection handling
    this._rpcServer.setServing(true);
    this._peerStore.startReaper();

    // Spawn the Gate 0 hook loop if hooks are enabled. This polls
    // before_connect events from iroh and applies the MeshEndpointHook's
    // allow/deny decisions. Without this, the admitted-set is never
    // consulted and every connection passes the QUIC handshake.
    if (this._node.hasHooks?.()) {
      void this._runGate0().catch(e => {
        this.logger.error('gate0 hook loop failed', { error: String(e) });
      });
    }

    // Single accept loop with ALPN routing (matches Python's _accept_loop)
    this._servePromise = this._acceptLoop();
    try {
      await this._servePromise;
    } catch (e) {
      if (!this._closed) throw e;
    }
  }

  /**
   * Run the Gate 0 hook loop.
   *
   * Polls after_handshake events from the native receiver and dispatches
   * each one through MeshEndpointHook.shouldAllow(). After-handshake fires
   * for **all** connections (inbound and outbound) right after TLS, which
   * is exactly when we want to enforce the admitted-set check.
   *
   * NOTE: We do NOT use before_connect — that fires only for *outgoing*
   * connections in iroh, so it would miss incoming RPC connections to
   * the server (which is what we need to gate).
   *
   * The native binding uses an event-id + respond-by-id API; this loop
   * polls events directly and responds via respondAfterHandshake().
   */
  private async _runGate0(): Promise<void> {
    if (!this._node) return;
    const receiver = this._node.takeHookReceiver();
    if (receiver == null) {
      this.logger.warn('Gate 0: hooks enabled but no receiver available');
      return;
    }
    this.logger.debug('Gate 0: hook loop started');

    try {
      while (true) {
        const event = await receiver.recvAfterHandshake();
        if (event == null) {
          this.logger.debug('Gate 0: receiver closed');
          return;
        }
        const alpnBytes = new Uint8Array(event.info.alpn);
        const peerId = event.info.remoteEndpointId;
        const allow = this._hook.shouldAllow(peerId, alpnBytes);
        if (allow) {
          receiver.respondHandshake(event.eventId, true, undefined, undefined);
        } else {
          this.logger.info(
            `Gate 0 denied ${peerId.slice(0, 12)} on alpn=${new TextDecoder().decode(alpnBytes)}`,
          );
          receiver.respondHandshake(event.eventId, false, 403, 'not admitted');
        }
      }
    } catch (e) {
      this.logger.error('Gate 0 hook loop error', { error: String(e) });
    }
  }

  /**
   * Stop accepting connections and close the node.
   */
  async close(): Promise<void> {
    if (this._closed) return;
    this._closed = true;
    this._running = false;

    // Remove signal handlers
    for (const cleanup of this._signalHandlers) cleanup();
    this._signalHandlers = [];

    if (this._admissionAbort) {
      this._admissionAbort.abort();
    }
    if (this._rpcServer) {
      await this._rpcServer.close();
    }
    await this.health.stop();
    if (this._node) {
      try { await this._node.close(); } catch { /* ignore */ }
    }
    this.logger.info('AsterServer stopped');
  }

  /** The aster1... connection address for clients. */
  get address(): string {
    if (!this._node) throw new Error('Server not started');
    try {
      const native = loadNativeSync();
      if (native) {
        const info: any = {
          endpointId: this._node.nodeId(),
          relayAddr: undefined,
          directAddrs: [],
        };
        return native.asterTicketToString(info);
      }
    } catch { /* fallback */ }
    return this._node.nodeId();
  }

  /** Hex endpoint ID of this server's node. */
  get endpointId(): string {
    if (!this._node) throw new Error('Server not started');
    return this._node.nodeId();
  }

  /** Whether the server is running. */
  get running(): boolean { return this._running; }

  /** List of services hosted by this server. */
  get services(): ServiceSummary[] { return [...this._serviceSummaries]; }

  /** Create a local in-process transport for testing. */
  localTransport(): LocalTransport {
    return new LocalTransport(this.registry);
  }

  // ── Admission loop ──────────────────────────────────────────────────────

  /**
   * Single accept loop with ALPN-based routing.
   *
   * All aster ALPNs (RPC, consumer admission, delegated admission) are
   * multiplexed through one channel. This loop reads the ALPN tag and
   * dispatches to the correct handler — matching Python's `_accept_loop`.
   */
  private async _acceptLoop(): Promise<void> {
    if (!this._node) return;

    while (this._running && !this._closed) {
      try {
        // Accept next connection — ALPN tag is on the connection
        const conn = await this._node.acceptAster();
        const alpn: string = conn.alpn() ?? '';

        if (alpn === RPC_ALPN) {
          // RPC connection — dispatch to RpcServer
          this._rpcServer!.handleConnection(conn).catch(e => {
            this.logger.error('rpc connection error', { error: String(e) });
          });
        } else if (alpn === ALPN_CONSUMER_ADMISSION) {
          // Consumer admission
          this._handleAdmission(conn).catch(e => {
            this.logger.error('admission error', { error: String(e) });
          });
        } else if (alpn === ALPN_PRODUCER_ADMISSION) {
          // Producer-to-producer mesh admission
          this._handleProducerAdmission(conn).catch(e => {
            this.logger.error('producer admission error', { error: String(e) });
          });
        } else if (alpn === ALPN_DELEGATED_ADMISSION) {
          // Delegated admission
          this._handleDelegatedAdmission(conn).catch(e => {
            this.logger.error('delegated admission error', { error: String(e) });
          });
        } else {
          this.logger.warn(`unknown ALPN: ${alpn}`);
          try { conn.close(400, 'unknown ALPN'); } catch { /* ignore */ }
        }
      } catch (e) {
        if (this._closed) return;
        this.logger.error('accept error', { error: String(e) });
      }
    }
  }

  private async _handleAdmission(conn: any): Promise<void> {
    // Adapt NAPI connection to the interface handleConsumerAdmissionConnection expects
    const adapted = {
      acceptBi: () => conn.acceptBi(),
      remoteId: () => conn.remoteNodeId?.() ?? conn.remoteId?.() ?? 'unknown',
    };
    const opts: ConsumerAdmissionOpts = {
      services: this._serviceSummaries,
      registryNamespace: this._registryNamespace || undefined,
      allowUnenrolled: this._allowAllConsumers,
      peerStore: this._peerStore,
      logger: this.logger,
    };
    const rootKeyHex = this._resolveRootPubkeyHex();
    await handleConsumerAdmissionConnection(
      adapted,
      rootKeyHex,
      this._hook,
      opts,
    );
  }

  // ── Producer admission ───────────────────────────────────────────────────

  private async _handleProducerAdmission(conn: any): Promise<void> {
    // Producer admission requires root pubkey and mesh state.
    // If not configured (open mode), reject gracefully.
    if (!this.config.rootPubkey && !this.config.rootPubkeyFile) {
      this.logger.warn('producer admission: no root pubkey configured, ignoring');
      return;
    }
    const { handleProducerAdmissionConnection } = await import('./trust/bootstrap.js');
    const rootKeyHex = this._resolveRootPubkeyHex();
    const { MeshState } = await import('./trust/mesh.js');
    const meshState = new MeshState();
    await handleProducerAdmissionConnection(conn, rootKeyHex, meshState);
  }

  private _resolveRootPubkeyHex(): string {
    if (this.config.rootPubkey) return Buffer.from(this.config.rootPubkey).toString('hex');
    if (this.config.rootPubkeyFile) {
      try {
        const { readFileSync } = require('node:fs');
        const expanded = this.config.rootPubkeyFile.replace(/^~/, process.env.HOME ?? '');
        const raw = readFileSync(expanded, 'utf-8').trim();
        if (raw.startsWith('{')) {
          try { return JSON.parse(raw).public_key; } catch { /* fall through */ }
        }
        return raw;
      } catch { return ''; }
    }
    return '';
  }

  // ── Delegated admission ─────────────────────────────────────────────────

  private async _handleDelegatedAdmission(conn: any): Promise<void> {
    const policy = this._delegationPolicies.values().next().value;
    if (!policy) return;
    await handleDelegatedAdmissionConnection(
      conn,
      { policy, hook: this._hook, peerStore: this._peerStore },
    );
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
        w(`    ${G}\u25cf${R} ${B}${name}${R}  ${D}v${s.version}${R}\n`);
      }
      w('\n');
    }

    // Endpoint
    try {
      const nodeId = this._node?.nodeId?.();
      if (nodeId) {
        const short = nodeId.slice(0, 16) + '\u2026';
        w(`    ${D}node id:${R}   ${W}${short}${R}  ${D}(this node's keypair fingerprint)${R}\n`);
      }
      w(`    ${D}endpoint:${R}  ${this.address}\n`);
    } catch { /* not started yet */ }

    // Mode
    const mode = this._allowAllConsumers ? `${Y}open-gate${R}` : `${G}trusted${R}`;
    w(`    ${D}mode:${R}      ${mode}\n`);

    // Log
    const logFormat = this.config.logFormat || 'text';
    const logLevel = this.config.logLevel || 'info';
    w(`    ${D}log:${R}       ASTER_LOG_FORMAT=${W}${logFormat}${R}  ASTER_LOG_LEVEL=${W}${logLevel}${R}\n`);

    // Runtime
    w(`    ${D}runtime:${R}   aster-rpc (typescript)  iroh 0.97\n`);

    // Copyright
    w(`\n    ${D}Copyright \u00a9 2026 Emrul Islam. All rights reserved.${R}\n\n`);
  }
}

// ── AsterClient ──────────────────────────────────────────────────────────────

/**
 * Options for AsterClientWrapper.
 * @group Server and Client
 */
export interface AsterClientOptions {
  /** Connection address (aster1... ticket, base64 NodeAddr, or hex EndpointId). */
  address?: string;
  /** @deprecated Use `address` instead. */
  endpointAddr?: string;
  transport?: AsterTransport;
  config?: Partial<AsterConfig>;
  /** Path to .aster-identity file. Overrides config.identityFile. */
  identity?: string;
  /** Peer name for identity file lookup. */
  peer?: string;
  /** Path to a pre-signed enrollment credential file (.cred JSON or .aster-identity TOML). */
  enrollmentCredentialFile?: string;
  /** Retry configuration for reconnection. */
  retryBackoff?: ExponentialBackoff;
  /** Pre-generated type metadata from aster-gen (same as AsterServer.generated). */
  generated?: {
    SERVICES: readonly any[];
    WIRE_TYPES: readonly any[];
    buildAllTypes?: (fory: any, Type: any, codec: any) => Map<string, any>;
  };
}

/**
 * Read the first consumer (or named) peer entry from a .aster-identity TOML file
 * without the synthesised attributes that loadIdentity adds.
 */
function loadRawConsumerPeer(
  filePath: string | undefined,
  peerName: string | undefined,
): Record<string, unknown> | null {
  const { existsSync, readFileSync } = require('node:fs');
  const { join } = require('node:path');
  const path = filePath ?? join(process.cwd(), '.aster-identity');
  if (!existsSync(path)) return null;
  try {
    const data = parseSimpleToml(readFileSync(path, 'utf-8'));
    const peers = (data.peers ?? []) as Record<string, unknown>[];
    if (peerName) {
      return peers.find(p => p.name === peerName) ?? null;
    }
    return peers.find(p => p.role === 'consumer') ?? peers[0] ?? null;
  } catch {
    return null;
  }
}

/**
 * Build a ConsumerEnrollmentCredential from a [[peers]] entry in .aster-identity.
 * Mirrors Python's `_credential_from_peer_entry`.
 */
function credentialFromPeerEntry(peer: Record<string, unknown>): ConsumerEnrollmentCredential {
  return {
    credentialType: ((peer.type as string) ?? 'policy') as 'policy' | 'ott',
    rootPubkey: peer.root_pubkey as string,
    expiresAt: Number(peer.expires_at),
    attributes: (peer.attributes ?? {}) as Record<string, string>,
    endpointId: (peer.endpoint_id as string) || undefined,
    nonce: (peer.nonce as string) || undefined,
    signature: (peer.signature as string) ?? '',
  };
}

/**
 * Load a pre-signed ConsumerEnrollmentCredential from a credential file.
 *
 * Accepts both formats produced by `aster enroll node`:
 *
 *   1. **TOML** (the actual `.cred` / `.aster-identity` format produced by
 *      the CLI today): a `[node]` section with the consumer's secret key,
 *      plus one or more `[[peers]]` sections each holding a signed
 *      enrollment credential. The first consumer-role peer is used.
 *   2. **JSON** (legacy / hand-rolled credential dumps): a flat object
 *      with credential_type / root_pubkey / expires_at / attributes /
 *      endpoint_id / nonce / signature.
 *
 * Format detection peeks at the first non-whitespace character: `{`
 * means JSON, anything else means TOML. The TOML path goes through the
 * existing identity-loader helpers so that `enrollmentCredentialFile:`
 * and `identity:` end up doing the same thing for the same file.
 */
function loadEnrollmentCredential(filePath: string): ConsumerEnrollmentCredential {
  const { readFileSync } = require('node:fs');
  const { homedir } = require('node:os');
  const expanded = filePath.startsWith('~')
    ? filePath.replace(/^~/, homedir())
    : filePath;
  const text = readFileSync(expanded, 'utf-8') as string;
  const firstChar = text.replace(/^\s+/, '').charAt(0);

  if (firstChar === '{') {
    // JSON path
    const d = JSON.parse(text);
    return {
      credentialType: (d.credential_type ?? d.type ?? 'policy') as 'policy' | 'ott',
      rootPubkey: d.root_pubkey,
      expiresAt: Number(d.expires_at),
      attributes: d.attributes ?? {},
      endpointId: d.endpoint_id || undefined,
      nonce: d.nonce || undefined,
      signature: d.signature ?? '',
    };
  }

  // TOML path -- reuse the identity helpers so behaviour matches `identity:`
  const data = parseSimpleToml(text);
  const peers = (data.peers ?? []) as Record<string, unknown>[];
  const consumerPeer = peers.find(p => p.role === 'consumer') ?? peers[0];
  if (!consumerPeer) {
    throw new Error(
      `loadEnrollmentCredential(${filePath}): no [[peers]] entry found in ` +
      `the TOML credential file. Did you run \`aster enroll node --role consumer\`?`,
    );
  }
  return credentialFromPeerEntry(consumerPeer);
}

/**
 * High-level Aster RPC client.
 *
 * Wraps connection setup, admission, and client stub creation.
 * Supports reconnection with exponential backoff.
 *
 * @group Server and Client
 */
export class AsterClientWrapper {
  private transport!: AsterTransport;
  readonly config: AsterConfig;
  private backoff: ExponentialBackoff;
  private _connected = false;
  private _gossipTopic = '';
  private _address: string | undefined;
  private _node: any = null;
  private _services: ServiceSummary[] = [];
  private _registryNamespace = '';
  private _inlineCredential: ConsumerEnrollmentCredential | null = null;
  private _enrollmentCredentialFile: string | undefined;
  private _identitySecretKey: Uint8Array | null = null;
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
    this.config = { ...configFromEnv(), ...opts.config } as AsterConfig;
    if (opts.identity) {
      this.config.identityFile = opts.identity;
    }
    this._address = opts.address ?? opts.endpointAddr as string | undefined;
    this.backoff = opts.retryBackoff ?? DEFAULT_BACKOFF;
    this._generated = opts.generated;

    // Load identity file (.aster-identity) if present. The first consumer-role
    // peer entry IS the credential — mirrors Python AsterClient behaviour.
    // The node secret_key is also pulled out so the client's QUIC endpoint id
    // matches the endpoint_id baked into the credential.
    //
    // We use loadIdentity for the secret key but parse the TOML raw for the
    // peer entry — loadIdentity synthesizes `aster.name` into attributes,
    // which would corrupt the signed-attribute set on the credential.
    const identity = loadIdentity(this.config.identityFile, opts.peer, 'consumer');
    if (identity) {
      this._identitySecretKey = identity.secretKey;
      if (!opts.enrollmentCredentialFile) {
        const rawPeer = loadRawConsumerPeer(this.config.identityFile, opts.peer);
        if (rawPeer) {
          this._inlineCredential = credentialFromPeerEntry(rawPeer);
        }
      }
    }
    this._enrollmentCredentialFile =
      opts.enrollmentCredentialFile ?? this.config.enrollmentCredentialFile;

    // If the user only passed `enrollmentCredentialFile` (no separate
    // `identity:`) and that file is a TOML `.aster-identity` produced by
    // `aster enroll node`, reach into the same file for the [node]
    // secret_key. Otherwise the QUIC endpoint id we generate at startup
    // won't match the credential's `endpoint_id` and admission fails
    // with no useful error.
    //
    // Both `enrollmentCredentialFile:` and `identity:` should do the
    // same thing for the same TOML file -- mirrors the matching fix on
    // the Python AsterClient.
    if (
      !this._identitySecretKey
      && this._enrollmentCredentialFile
      && !identity
    ) {
      const credIdentity = loadIdentity(
        this._enrollmentCredentialFile,
        opts.peer,
        'consumer',
      );
      if (credIdentity) {
        this._identitySecretKey = credIdentity.secretKey;
      }
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

  /** Services discovered during admission. */
  get services(): ServiceSummary[] { return [...this._services]; }

  /** Registry namespace ID for service discovery (set after admission). */
  get registryNamespace(): string | undefined { return this._registryNamespace || undefined; }

  /** Hex-encoded 32-byte gossip topic ID for the producer mesh. */
  get gossipTopic(): string { return this._gossipTopic; }

  /**
   * Connect to the server via consumer admission, then open an RPC transport.
   *
   * If the client was created with a transport, this is a no-op.
   * If created with an address, it performs the full admission handshake.
   */
  async connect(): Promise<void> {
    if (this._connected) return;
    if (!this._address) {
      throw new Error(
        'AsterClient requires an address or transport. ' +
        'Pass address="aster1..." or transport=new IrohTransport(conn).',
      );
    }

    const native = await loadNative();
    if (!native) {
      throw new Error('Aster native addon not found.');
    }

    // Create an in-memory client node. When an identity file provided a
    // secret key, pass it through so the client's endpoint id matches the
    // credential's enrolled endpoint_id.
    if (this._identitySecretKey) {
      this._node = await native.IrohNode.memoryWithAlpns(
        [Buffer.from(ALPN_CONSUMER_ADMISSION), Buffer.from(RPC_ALPN)],
        { secretKey: Buffer.from(this._identitySecretKey) },
      );
    } else {
      this._node = await native.IrohNode.memory();
    }

    // Parse the address to get the endpoint ID and optional address hints.
    //
    // The ticket's `relayAddr` is a SocketAddr (STUN-discovered public IP),
    // NOT an iroh RelayUrl. Treat it as another direct addr — iroh will use
    // any reachable transport address it can.
    let endpointId: string;
    let allDirectAddrs: string[] = [];

    if (this._address.startsWith('aster1')) {
      let parsed: any;
      try {
        parsed = native.asterTicketFromString(this._address);
      } catch {
        parsed = native.asterTicketDecode(Buffer.from(this._address));
      }
      endpointId = parsed.endpointId;
      if (parsed.directAddrs?.length) allDirectAddrs.push(...parsed.directAddrs);
      if (parsed.relayAddr) allDirectAddrs.push(parsed.relayAddr);
    } else {
      // Treat as raw hex endpoint ID
      endpointId = this._address;
    }

    this._remoteEndpointId = endpointId;

    const directAddrs = allDirectAddrs.length ? allDirectAddrs : undefined;

    // Helper: connect using full address info when available, else bare endpoint ID
    const doConnect = (alpn: Buffer) => {
      if (directAddrs) {
        // No relay URL — iroh's connect_node_addr handles direct addresses
        return this._node.connectNodeAddr(endpointId, alpn, directAddrs, undefined);
      }
      return this._node.connect(endpointId, alpn);
    };

    // Build credential: inline peer entry > credential file > null (open-gate).
    let credential: ConsumerEnrollmentCredential | null = this._inlineCredential;
    let credentialFileLabel: string | null = null;
    if (credential) {
      credentialFileLabel = '<inline .aster-identity peer entry>';
    } else if (this._enrollmentCredentialFile) {
      credential = loadEnrollmentCredential(this._enrollmentCredentialFile);
      credentialFileLabel = this._enrollmentCredentialFile;
    }

    // Consumer admission
    const admissionConn = await doConnect(Buffer.from(ALPN_CONSUMER_ADMISSION));
    const admissionResponse = await performAdmission(admissionConn, credential as any);

    if (!admissionResponse.admitted) {
      let ourEndpointId = '';
      try {
        ourEndpointId = this._node?.nodeId?.() ?? '';
      } catch { /* ignore */ }
      throw new AdmissionDeniedError({
        hadCredential: credential != null,
        credentialFile: credentialFileLabel,
        ourEndpointId,
        serverAddress: this._address ?? '',
      });
    }

    this._services = admissionResponse.services ?? [];
    this._registryNamespace = admissionResponse.registryNamespace ?? '';
    this._gossipTopic = admissionResponse.gossipTopic ?? '';

    // Pick codec based on server's advertised serialization modes.
    // JsonCodec when all services are JSON-only; ForyCodec otherwise.
    const allJsonOnly = this._services.every((svc) => {
      const modes = svc.serializationModes;
      if (!modes || modes.length === 0) return false;
      return modes.includes('json') && !modes.includes('xlang');
    });
    const codec = allJsonOnly ? new JsonCodec() : createXlangCodec();
    this._codec = codec;

    // If ForyCodec and generated metadata provided, register wire types
    // so the client can encode requests without "Failed to detect Fory type" errors.
    if (!allJsonOnly && this._generated?.buildAllTypes) {
      const { fory, Type } = getXlangForyAndType();
      this._generated.buildAllTypes(fory, Type, codec);
    }

    // Open RPC connection and create transport
    const rpcConn = await doConnect(Buffer.from(RPC_ALPN));
    const { IrohTransport: IrohTx } = await import('./transport/iroh.js');
    this.transport = new IrohTx(rpcConn, codec);
    this._connected = true;
  }

  /** Create a typed client proxy for a service class. */
  async client<T extends new (...args: any[]) => any>(serviceClass: T): Promise<ClientProxy<InstanceType<T>>> {
    return createClient(serviceClass, this.transport);
  }

  /** Create a typed client proxy for a service class. */
  service<T extends new (...args: any[]) => any>(serviceClass: T): ClientProxy<InstanceType<T>> {
    return createClient(serviceClass, this.transport);
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
    const summary = this._services.find(s => s.name === serviceName);
    const hintTypes = await this._registerDynamicTypesForService(summary);
    if (summary && (summary.pattern === 'session' || summary.pattern === 'stream')) {
      return new SessionProxyClient(
        serviceName, this._node, this.transport, this._codec, hintTypes,
      ) as any;
    }
    return new ProxyClient(serviceName, this.transport, hintTypes);
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
        const dc = this._node.docsClient();
        const result = await dc.joinAndSubscribeNamespace(
          this._registryNamespace,
          this._remoteEndpointId,
        );
        const doc = result.takeDoc();
        const events = result.takeEvents();

        // Best-effort sync wait: bail out when sync_finished arrives or a
        // contracts/ entry shows up, whichever comes first. Short total
        // deadline because the producer writes these synchronously on
        // startup so the initial pull is usually quick.
        const deadline = Date.now() + 6000;
        while (Date.now() < deadline) {
          const remaining = deadline - Date.now();
          if (remaining <= 0) break;
          const waitMs = Math.min(remaining, 1000);
          const ev = await Promise.race<any>([
            events.recv(),
            new Promise<null>(r => setTimeout(() => r(null), waitMs)),
          ]);
          if (ev && ev.kind === 'sync_finished') {
            await new Promise(r => setTimeout(r, 100));
            break;
          }
          if (!ev) {
            try {
              const entries: string[] = await doc.queryKeyPrefix('contracts/');
              if (entries.length) break;
            } catch {
              /* ignore */
            }
          }
        }
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
      const entries: string[] = await doc.queryKeyExact(manifestKey(summary.contractId));
      if (entries.length) {
        const contentHash = entries[0].split(':').slice(-1)[0];
        const bytes: Buffer = await doc.readEntryContent(contentHash);
        manifest = manifestFromJson(new TextDecoder().decode(bytes));
      }
    } catch {
      /* fall through to slow path */
    }

    // Slow path: read ArtifactRef → fetch collection from blobs.
    if (!manifest) {
      try {
        const entries: string[] = await doc.queryKeyExact(contractKey(summary.contractId));
        if (entries.length) {
          const contentHash = entries[0].split(':').slice(-1)[0];
          const bytes: Buffer = await doc.readEntryContent(contentHash);
          const artifactRef = JSON.parse(new TextDecoder().decode(bytes));
          const collectionHash = artifactRef.collection_hash;
          if (collectionHash) {
            const { fetchContract } = await import('./contract/publication.js');
            const blobs = this._node.blobsClient();
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
   * Register request/response types for a service against the live Fory
   * codec, so dynamic proxy / session calls encode as Fory XLANG instead
   * of failing with "Failed to detect Fory type" on a plain object.
   * No-op when the service advertises json-only, or when the codec is
   * not Fory (shell-style JsonCodec path).
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
        factory.registerWithFory(methods, fory, Type, this._codec);
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

  /** Close the client and underlying transport. */
  async close(): Promise<void> {
    this._connected = false;
    await this.transport.close();
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
        return _proxyMethod(target.serviceName, prop, target.transport, hintType);
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
) {
  // The callable: detects async iterables for client streaming, else unary
  const fn = async (payload?: unknown) => {
    // Detect client streaming: payload is an async iterable (but not a plain object)
    if (payload != null && typeof payload === 'object' && Symbol.asyncIterator in payload) {
      const wrapped = hintType
        ? _wrapAsyncIterable(payload as AsyncIterable<unknown>, hintType)
        : (payload as AsyncIterable<unknown>);
      return transport.clientStream(serviceName, methodName, wrapped);
    }
    // Default: unary. If the user accidentally calls a server-streaming
    // method via `await proxy.method(...)`, the underlying transport will
    // see a second response frame and throw with "multiple response
    // frames". Catch that and re-raise with an actionable hint pointing
    // at `proxy.method.stream(...)` / `.bidi()`.
    try {
      return await transport.unary(serviceName, methodName, payload ?? {}, { hintType });
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
    return transport.serverStream(serviceName, methodName, payload ?? {}, { hintType });
  };

  // .bidi() — bidirectional streaming. Returns a lazy wrapper so callers
  // can do `const ch = m.bidi(); await ch.open(); await ch.send(...)` —
  // mirrors Python's _ProxyBidiChannel behaviour. Opening eagerly here
  // would force every call site to handle the connect failure synchronously.
  fn.bidi = (): ProxyBidiChannel => {
    return new ProxyBidiChannel(serviceName, methodName, transport, hintType);
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
  ) {}

  async open(): Promise<void> {
    if (this._channel) return;
    this._channel = this.transport.bidiStream(this.serviceName, this.methodName);
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
      this._channel = this.transport.bidiStream(this.serviceName, this.methodName);
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
  private static _sessionIdCounter = 0;
  private readonly _hintTypes?: Map<string, new (init?: any) => any>;

  constructor(
    private readonly serviceName: string,
    _node: any,
    transport: AsterTransport,
    codec?: any,
    hintTypes?: Map<string, new (init?: any) => any>,
  ) {
    this._hintTypes = hintTypes;
    // Reach through the v1 transport to get its underlying QUIC
    // connection. We build a fresh transport on the same connection
    // but pinned to a freshly-allocated sessionId, so each outbound
    // call carries `StreamHeader.sessionId=N` without disturbing
    // other callers of the original transport.
    const conn = (transport as any).conn ?? (transport as any)._conn;
    if (!conn) {
      throw new RpcError(
        StatusCode.UNAVAILABLE,
        'SessionProxyClient: v1 transport did not expose its connection',
      );
    }
    const sessionId = ++SessionProxyClient._sessionIdCounter;
    // Reuse the parent transport's codec so manifest-synthesised types
    // (registered via DynamicTypeFactory at proxy() time) are visible on
    // this session's stream header frames too. Fall back to the raw
    // codec on the transport when one isn't supplied explicitly.
    const activeCodec = codec ?? (transport as any).codec ?? new JsonCodec();
    this._sessionTransport = new IrohTransport(conn, activeCodec, { sessionId });

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
        { hintType },
      );
    };
    fn.bidi = (): ProxyBidiChannel => {
      throw new RpcError(
        StatusCode.UNIMPLEMENTED,
        'session proxy bidi not yet implemented',
      ) as any;
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
    return await this._sessionTransport.unary(this.serviceName, method, request, { hintType });
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

// ── Native addon loader ──────────────────────────────────────────────────────

let _native: any = null;

function loadNativeSync(): any {
  if (_native) return _native;

  const { resolve } = require('node:path');
  const { existsSync } = require('node:fs');

  const platforms = [
    'aster-transport.darwin-arm64.node',
    'aster-transport.darwin-x64.node',
    'aster-transport.linux-x64-gnu.node',
    'aster-transport.linux-arm64-gnu.node',
    'aster-transport.win32-x64-msvc.node',
  ];

  // 1. Try the workspace package
  try {
    _native = require('@aster-rpc/transport');
    return _native;
  } catch { /* next */ }

  // 2. Try ASTER_NATIVE_PATH env var
  const envPath = process.env.ASTER_NATIVE_PATH;
  if (envPath) {
    for (const name of platforms) {
      const full = resolve(envPath, name);
      if (existsSync(full)) { _native = require(full); return _native; }
    }
  }

  // 3. Try common workspace layouts relative to this file
  const searchDirs: string[] = [];
  try {
    const { dirname } = require('node:path');
    // When loaded from packages/aster/src/
    const thisDir = typeof __dirname !== 'undefined'
      ? __dirname
      : dirname(new URL(import.meta.url).pathname);
    searchDirs.push(resolve(thisDir, '../../../native'));    // packages/aster/src -> native
    searchDirs.push(resolve(thisDir, '../../native'));       // packages/aster -> native
    searchDirs.push(resolve(process.cwd(), 'bindings/typescript/native')); // repo root
    searchDirs.push(resolve(process.cwd(), 'node_modules/@aster-rpc/transport')); // linked
  } catch { /* ignore */ }

  for (const dir of searchDirs) {
    for (const name of platforms) {
      const full = resolve(dir, name);
      try {
        if (existsSync(full)) { _native = require(full); return _native; }
      } catch { /* next */ }
    }
  }

  return null;
}

async function loadNative(): Promise<any> {
  return loadNativeSync();
}
