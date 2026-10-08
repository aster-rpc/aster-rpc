/**
 * @aster-rpc/aster -- Aster RPC framework for TypeScript.
 *
 * P2P services with type safety, streaming, and trust.
 * Built on Iroh (QUIC, blobs, docs, gossip).
 *
 * Works with Node.js 20+, Bun 1.0+, and Deno (via Node compat).
 * All examples use TypeScript; plain JavaScript works by omitting type annotations.
 *
 * @packageDocumentation
 */

// Status codes and errors
export {
  StatusCode,
  statusName,
  RpcError,
  CancelledError,
  UnknownRpcError,
  InvalidArgumentError,
  DeadlineExceededError,
  NotFoundError,
  AlreadyExistsError,
  PermissionDeniedError,
  ResourceExhaustedError,
  FailedPreconditionError,
  AbortedError,
  OutOfRangeError,
  UnimplementedError,
  InternalError,
  UnavailableError,
  DataLossError,
  UnauthenticatedError,
  ContractViolationError,
} from './status.js';

// Types and enums
export {
  SerializationMode,
  RpcPattern,
  RpcScope,
  RPC_ALPN,
  DEFAULT_BACKOFF,
  DEFAULT_RETRY,
  type ExponentialBackoff,
  type RetryPolicy,
} from './types.js';

// Security limits
export {
  MAX_FRAME_SIZE,
  MAX_DECOMPRESSED_SIZE,
  DEFAULT_FRAME_READ_TIMEOUT_S,
  MAX_METADATA_ENTRIES,
  MAX_METADATA_TOTAL_BYTES,
  MAX_STATUS_MESSAGE_LEN,
  HEX_FIELD_LENGTHS,
  LimitExceeded,
  validateHexField,
  validateMetadata,
  validateStatusMessage,
} from './limits.js';

// Framing
export {
  COMPRESSED,
  TRAILER,
  HEADER,
  ROW_SCHEMA,
  CALL,
  CANCEL,
  END_STREAM,
  FramingError,
  encodeFrame,
  encodePayloadFrame,
  decodeFrame,
  payloadOf,
  type FrameResult,
} from './framing.js';

// Protocol envelope (AEv1, Aster-SPEC §6.9; encoded by Rust core)
export {
  StreamHeader,
  CallHeader,
  RpcStatus,
  encodeStreamHeader,
  decodeStreamHeader,
  encodeCallHeader,
  decodeCallHeader,
  encodeRpcStatus,
  decodeRpcStatus,
} from './protocol.js';

// Service metadata and registry
export {
  SERVICE_INFO_KEY,
  METHOD_INFO_KEY,
  getServiceInfo,
  ServiceRegistry,
  getMethod,
  hasMethod,
  getDefaultRegistry,
  setDefaultRegistry,
  type ServiceInfo,
  type MethodInfo,
  type CapabilityRequirement,
} from './service.js';

// Metadata
export { Metadata } from './metadata.js';

// Decorators
export {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  WireType,
  ContractDefault,
  getContractDefaults,
  WIRE_TYPE_KEY,
  WIRE_TYPE_FIELDS_KEY,
  CONTRACT_DEFAULTS_KEY,
  type ServiceOptions,
  type WireTypeOptions,
} from './decorators.js';

// Branded numeric types for explicit wire selection (spec §11.3.2.3)
export {
  i8, i16, i32, i64,
  u8, u16, u32, u64,
  f32, f64,
  BRAND_TO_WIRE,
  type AsterBrandTag,
} from './brand.js';

// Generated-file glue (for files emitted by `aster-gen`)
export {
  registerGenerated,
  getWireShape,
  getGeneratedMethodFields,
  type WirePrimitive,
  type WireFieldShape,
  type WireTypeShape,
  type GeneratedMethodDef,
  type GeneratedServiceDef,
  type RegisterGeneratedOptions,
  type GeneratedCodec,
} from './generated.js';

// Codec
export {
  ForyCodec,
  DEFAULT_COMPRESSION_THRESHOLD,
  walkTypeGraph,
  wireType,
  type Codec,
} from './codec.js';

// XLANG Fory helpers (the one Fory configuration: xlang, compatible, ref off)
export {
  createXlangCodec,
  getXlangForyAndType,
  newXlangFory,
  FORY_VERSION,
  FORY_OPTIONS,
} from './xlang.js';

// Canonical wire primitives + contract-default lowering (§11.3.2.3)
export {
  CANONICAL_PRIMITIVES,
  encodeContractDefault,
  foryTypeFor,
  type CanonicalPrimitive,
} from './wire-types.js';

// Simple trust-config primitives (Rust core + Fory XLANG)
export {
  HPKE_ENVELOPE_ALG,
  NamespaceCapability,
  HpkeEnvelope,
  namespaceSecretId,
  hpkeGenerateKeypair,
  hpkePublicKeyFromPrivate,
  hpkeSeal,
  hpkeOpen,
  setNativeTrustConfigPrimitives,
  type HpkeKeyPair,
} from './trust/config-primitives.js';

// Transport
export {
  type AsterTransport,
  type CallOptions,
  type BidiChannel,
} from './transport/base.js';
export { LocalTransport, MemRecvStream } from './transport/local.js';

// Client
export {
  createClient,
  createLocalClient,
  timeSleep,
  timeouts,
  ServiceClient,
  type AsterClient,
  type ClientOptions,
} from './client.js';

// Interceptors
export {
  CallContext,
  buildCallContext,
  applyRequestInterceptors,
  applyResponseInterceptors,
  applyErrorInterceptors,
  normalizeError,
  type Interceptor,
  type PeerAddr,
} from './interceptors/base.js';
export { RetryInterceptor } from './interceptors/retry.js';
export { RateLimitInterceptor, type RateLimitOptions } from './interceptors/rate-limit.js';
export { AuthInterceptor } from './interceptors/auth.js';
export { CircuitBreakerInterceptor, type CircuitBreakerOptions } from './interceptors/circuit-breaker.js';
export { CompressionInterceptor } from './interceptors/compression.js';
export { AuditLogInterceptor, type AuditLogFn, type AuditEntry } from './interceptors/audit.js';
export { CapabilityInterceptor } from './interceptors/capability.js';

// Contract identity (delegated to Rust core via NAPI)
export {
  canonicalXlangBytes,
  computeContractId,
  contractIdFromContract,
  contractIdFromJson,
  contractIdFromService,
  buildTypeGraph,
  fromServiceInfo,
  setNativeContract,
  decodeTypeDefBytes,
  TypeKind as ContractTypeKind,
  ContainerKind,
  TypeDefKind,
  MethodPattern,
  CapabilityKind,
  ScopeKind,
  type ServiceContract,
  type MethodDef as ContractMethodDef,
  type CapabilityRequirement as ContractCapabilityRequirement,
  type CanonicalTypeDef,
  type CanonicalFieldDef,
  type CanonicalEnumValueDef,
  type CanonicalUnionVariantDef,
} from './contract/identity.js';

// Contract manifest and publication
export {
  type ContractManifest,
  type ManifestMethod,
  type ManifestField,
  FatalContractMismatch,
  verifyManifestOrFatal,
  manifestToJson,
  manifestFromJson,
  extractMethodDescriptors,
  saveManifest,
} from './contract/manifest.js';
export {
  type ArtifactRef,
  buildCollection,
  publishContract,
  uploadCollection,
  fetchFromCollection,
  fetchContract,
} from './contract/publication.js';

// Dynamic type factory
export {
  DynamicTypeFactory,
  createDynamicType,
  type DynamicType,
} from './dynamic.js';

// Transport implementations
export { IrohTransport } from './transport/iroh.js';

// Node + configuration (Rust `aster::Node` / `aster::AsterConfig`)
export { Node, NodeId, type NodeAddr, type NodeStartOptions, type LeaseGrantorOptions } from './node.js';
// Attestation, identity-derived HPKE keys, sealed grants, leases, topology (plan T7)
export {
  AttestationChain,
  AttestationError,
  VerifiedChain,
  attestRootNode,
  verifyChain,
  attestationPublicKey,
  type AttestOptions,
  type AttestationRole,
} from './attestation.js';
export {
  hpkeX25519SecretFromIdentity,
  hpkeX25519PublicFromIdentity,
  hpkeX25519PublicFromNodeId,
} from './crypto.js';
export {
  GRANT_AAD_DOMAIN,
  GRANT_RECORD_VERSION,
  GrantRecord,
  PolicyDoc,
  grantAad,
  sealGrant,
  openGrant,
  sealGrantToKey,
  openGrantWithKey,
  sealNamespaceGrant,
  openNamespaceGrant,
  checkGrantRole,
  grantKey,
  reconcileGrant,
  reconcileNamespaceGrant,
  type GrantContext,
  type GrantOutcome,
  type GrantState,
} from './grants.js';
export {
  Lease,
  MemorySerializer,
  GrantorSerializer,
  FenceRejectedError,
  leaseTransition,
  checkFence,
  type Fence,
  type LeaseSnapshot,
  type LeaseOptions,
  type LeaseStatus,
  type DenyReason,
  type AcquireOutcome,
  type TransitionOutcome,
  type LeaseRowState,
  type LeaseOp,
  type LeaseTransition,
  type LeaseSerializer,
  type FenceRejectionKind,
  ForeignLeaseSerializer,
  ForeignFencedResource,
  FencedOutcomeUnknownError,
  type LeaseBackend,
  type LeaseAuthorizer,
  type LeaseAuthorizerFn,
  type FencedResource,
  type ForeignLeaseSerializerOptions,
} from './lease.js';
// Coordination client (Rust `aster::coordination::client`, wrapped — plan T8)
export {
  CoordinationClient,
  CoordinationReadClient,
  CoordinationLease,
  ResolvedMutation,
  CoordinationError,
  CoordinationAwaitingAckError,
  CoordinationUnresolvedError,
  CoordinationJournalFatalError,
  CoordinationJournalLockedError,
  CoordinationRpcError,
  CoordinationUnavailableError,
  CoordinationClosedError,
  CoordinationRejectedError,
  COORDINATION_OUTCOMES,
  COORDINATION_REASONS,
  CoordinationRights,
  MAX_AUDIT_PAGE,
  fenceOf,
  placementPlan,
  newOperationId,
  type CoordinationOutcome,
  type CoordinationReason,
  type CoordinationErrorKind,
  type CoordinationFence,
  type CoordinationClientOptions,
  type CoordinationReadClientOptions,
  type CoordinationLeaseOutcome,
  type ResourceKey,
  type ResourceKeyInput,
  type ResourceSnapshot,
  type MutationReply,
  type LogId,
  type MemberSeed,
  type MembershipChange,
  type AuditStreamOptions,
  type AclStatus,
  type AdminStatus,
  type AuditRecord,
  type ClientProbe,
  type ExportManifest,
  type GroupStatus,
  type MembershipStatus,
  type RetireReply,
  type SeparationSummary,
  type PlacementCandidate,
  type PlacementPolicy,
  type PlacementInput,
  type PlacementPlan,
  type PlacementRecommendation,
  PlacementManager,
  type PlacementManagerOptions,
  type PlacementPolicySource,
  type PlacementProvisioner,
  type ProvisionRequest,
  type TopologySource,
  type TopologyView,
  type PlacementReport,
} from './coordination.js';
export {
  Topology,
  type LadderLevel,
  type SeparationVerdict,
  type PeerView,
  type ClusterView,
  type TopologySnapshot,
  type TopologyTiming,
  type TopologySwarmOptions,
  AdmittedFilter,
  type AdmittedFilterFn,
  type AdmittedFilterOptions,
  type AdmittedFilterStats,
} from './topology.js';
// Upcalls: Rust extension points implemented in JavaScript (plan R6)
export {
  Upcalls,
  UpcallCancelledError,
  UPCALL_KINDS,
  UPCALL_STATUS,
  upcallCodecDecode,
  upcallCodecEncode,
  type UpcallKindName,
  type UpcallCancelReason,
  type UpcallFailureMode,
  type UpcallPolicy,
  type UpcallsOptions,
  type UpcallContext,
  type UpcallStats,
} from './upcalls.js';
// Expose (Rust `aster::expose`): socket targets, JavaScript HTTP handlers and
// edge route policies, with JavaScript authorization
export {
  EdgeRouter,
  Expose,
  HttpBodyError,
  HttpBodyStream,
  HttpRelayBody,
  HttpRequest,
  HttpStreamPending,
  HttpStreamResponse,
  HttpUpload,
  RouteRegistration,
  relayHttp,
  relayHttpStream,
  relayHttpStreaming,
  requestRoute,
  type EdgeRoute,
  type ExposeAuthorizer,
  type ExposeAuthorizeRequest,
  type ExposeHttpOptions,
  type HttpBodyChunk,
  type HttpHandler,
  type HttpHeader,
  type HttpHeadersInit,
  type HttpResponse,
  type HttpResponseBody,
  type HttpVersion,
  type RelayHttpRequest,
  type RelayHttpStreamingOptions,
  type RelayHttpStreamOptions,
  type RelayedHttpResponse,
  type RoutePolicy,
  type RoutePolicyRequest,
  type RouteSpec,
} from './expose.js';
// Data plane (Rust `aster::{Blobs, Docs, Gossip}` and connection paths)
export {
  type BlobFormat,
  type FetchStrategy,
  type ShareMode,
  type LocalAccess,
  type DownloadPolicyMode,
  type DocEventKind,
  type GossipEventKind,
  authorityFingerprint,
  defaultAuthorId,
} from './data-plane.js';
export type {
  AdmissionRequests,
  AuthorityReport,
  BlobStatusInfo,
  BlobsClient,
  CollectionFetchResult,
  DocEntry,
  DocEvent,
  DocEventReceiver,
  DocHandle,
  DocsClient,
  DocWithEvents,
  DownloadPolicy,
  FetchOptions,
  FetchReport,
  FetchResult,
  GossipClient,
  GossipEvent as GossipTopicEvent,
  GossipTopicHandle,
  IrohConnection,
  IrohSendStream,
  IrohRecvStream,
  PathInfo,
  ProviderOutcome,
  RetireOutcome,
} from '@aster-rpc/transport';
export {
  AsterConfig,
  resolveRootPubkey,
  loadIdentity,
  getProducerTokens,
  printConfig,
  type AsterIdentity,
  type IdentityPeer,
  type RelayModeName,
  type HookFailureMode,
  type LogFormat,
  type LogLevel,
  type KeyMaterial,
} from './config.js';
export { SecretKey } from './secret-key.js';

// Logging
export {
  AsterLogger,
  createLogger,
  withRequestContext,
  getRequestContext,
  emitLog,
  type EmitLevel,
  type RequestContext,
  type LoggerOptions,
} from './logging.js';

// Observability (design OBSERVABILITY_HEALTH_DESIGN.md O7): pipeline, Rust log
// records, custom metrics, tracing and the Rust health listener.
export {
  initObservability,
  ensureObservability,
  shutdownObservability,
  flushObservability,
  setLogSink,
  setLogDelivery,
  setLogFilter,
  pollLogs,
  logStats,
  prometheusText,
  consoleSink,
  pinoSink,
  winstonSink,
  LOG_LEVELS,
  LogFlag,
  type LogSink,
  type LogDeliveryOptions,
  type RustLogRecord,
  type LogStats,
  type ObservabilityOptions,
  type PinoLike,
  type WinstonLike,
} from './observability.js';
export {
  metrics,
  Counter,
  UpDownCounter,
  Histogram,
  Gauge,
  AttributeSet,
  type Attributes,
  type InstrumentOptions,
} from './metrics.js';
export {
  tracing,
  Span,
  span,
  parseTraceparent,
  formatTraceparent,
  currentTraceContext,
  currentTraceId,
  currentSpanId,
  currentBaggage,
  currentTraceparent,
  withTraceContext,
  traceMetadata,
  tracingMixin,
  winstonTracingFormat,
  type TraceContext,
  type SpanKind,
  type SpanAttributes,
  type SpanOptions,
} from './tracing.js';
export type { HealthServer, ReadinessCheckHandle } from '@aster-rpc/transport';

// High-level API
export {
  AsterServer,
  AsterClientWrapper,
  ProxyClient,
  AdmissionDeniedError,
  type AsterServerOptions,
  type AsterClientOptions,
} from './runtime.js';

// @aster endpoint registration
export {
  loadProducerTokens,
  resolveAsterAddress,
  startRegistrationLoop,
  type ProducerTokenEntry,
  type NodeInfo,
  type RegistrationLoopOptions,
  type RegistrationTransport,
} from './registration.js';

// Capability helpers
export { anyOf, allOf } from './capabilities.js';

// Trust & Security
export {
  generateKeypair,
  generateRootKeypair,
  loadPrivateKey,
  loadPublicKey,
  verifySignature,
  sign,
  verify,
  ATTR_ROLE,
  ATTR_NAME,
  type EnrollmentCredential,
  type ConsumerEnrollmentCredential,
  canonicalJson,
  producerSigningBytes,
  consumerSigningBytes,
  credentialSigningBytes,
  signCredential,
  verifyCredentialSignature,
  hexToBytes,
  bytesToHex,
} from './trust/credentials.js';
export {
  verifyConsumerCredential,
  verifyProducerCredential,
  checkOffline,
  checkRuntime,
  admit,
  type AdmissionResult,
  type AdmitOptions,
} from './trust/admission.js';
export {
  PRODUCER_ADMISSION_ALPN,
  CONSUMER_ADMISSION_ALPN,
  DELEGATED_ADMISSION_ALPN,
  gateAdmitter,
  type PeerAdmitter,
} from './trust/hooks.js';
// Gate 0 (Rust `GatePolicy` / `PeerAdmissionStore`)
export {
  GatePolicy,
  PeerAdmissionStore,
  type TrustMode,
  type AdmissionSource,
  type AdmissionRequest,
  type AdmitOptions as PeerAdmitOptions,
  type GateDecision,
  type GatePolicyOptions,
  type PeerAdmission,
  type AdmissionHooks,
  type AdmissionHookRequest,
  type AdmissionRejection,
} from './gate.js';
// Nonce store
export {
  InMemoryNonceStore,
  type NonceStore,
} from './trust/nonce.js';

// RCAN validation
export {
  evaluateCapability,
  extractCallerRoles,
  validateRcan,
  encodeRcan,
  decodeRcan,
} from './trust/rcan.js';

// IID (cloud identity)
export {
  verifyIID,
  getIIDBackend,
  MockIIDBackend,
  AWSIIDBackend,
  GCPIIDBackend,
  AzureIIDBackend,
  ATTR_IID_PROVIDER,
  ATTR_IID_ACCOUNT,
  ATTR_IID_REGION,
  ATTR_IID_ROLE_ARN,
  type IIDBackend,
} from './trust/iid.js';

// RPC server on `aster::rpc::Server` (dynamic dispatch, design §4.3)
export { RpcServer, type ServerOptions } from './dispatch.js';
export {
  DispatchLoop,
  DispatchEventKind,
  DispatchFlags,
  CancelReason as DispatchCancelReason,
  SubmitResult,
  DEFAULT_DISPATCH_DELIVERY,
  DISPATCH_DELIVERIES,
  type DispatchDelivery,
  type DispatchLoopOptions,
  type DispatchServerTuning,
} from './dispatch-server.js';
export {
  lowerService,
  lowerServiceJson,
  dynamicContractId,
  normalizeRequirement,
  type DynamicServiceSpec,
  type DynamicMethodSpec,
  type ServiceLike,
} from './lowering.js';

// Registry
export {
  RegistryClient,
  registryKey,
  type RegistryArtifactRef,
  type EndpointLease as RegistryEndpointLease,
} from './registry/client.js';
export {
  RegistryPublisher,
  type RegistryPublisherOptions,
} from './registry/publisher.js';
export {
  RegistryACL,
} from './registry/acl.js';
export {
  RegistryGossip,
} from './registry/gossip.js';
export {
  contractKey,
  versionKey,
  channelKey,
  tagKey,
  leaseKey,
  leasePrefix,
  aclKey,
  configKey,
  REGISTRY_PREFIXES,
} from './registry/keys.js';
export {
  HealthStatus,
  GossipEventType,
  validate as validateHealthStatus,
  isLeaseFresh,
  isLeaseRoutable,
  type ServiceSummary as RegistryServiceSummary,
  type ArtifactRef as RegistryArtifactModel,
  type EndpointLease,
  type GossipEvent,
} from './registry/models.js';

// Consumer admission
export {
  performAdmission,
  consumerCredToJson,
  consumerCredFromJson,
  handleConsumerAdmissionRpc,
  handleConsumerAdmissionConnection,
  serveConsumerAdmission,
  type ConsumerAdmissionRequest,
  type ConsumerAdmissionResponse,
  type ConsumerAdmissionOpts,
  type ServiceSummary,
} from './trust/consumer.js';

// Delegated admission (aster.admission ALPN)
export {
  verifyAttestation,
  verifyToken,
  verifyProofOfPossession,
  buildChallengeBytes,
  handleDelegatedAdmissionConnection,
  type SigningKeyAttestation,
  type EnrollmentToken,
  type DelegatedAdmissionPolicy,
  type DelegatedAdmissionResult,
  type BiStream,
  type AdmissionConnection,
} from './trust/delegated.js';

// Explicit durable audit submission (shared Rust outbox).
export {
  AuditOutbox, AuditOutboxError,
  type AuditOutboxErrorKind, type AuditDestination, type AuditOutboxConfig,
  type AuditEvent, type AuditJson, type AuditOutboxStatus, type AuditDeliveryError,
  type AuditDispositionReceipt, type AuditQuarantineExport,
} from './audit-outbox.js';
