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
  FramingError,
  writeFrame,
  readFrame,
  encodeFrame,
  decodeFrame,
  type FrameResult,
} from './framing.js';

// Protocol types
export { StreamHeader, CallHeader, RpcStatus } from './protocol.js';

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
  WIRE_TYPE_KEY,
  WIRE_TYPE_FIELDS_KEY,
  type ServiceOptions,
  type WireTypeOptions,
} from './decorators.js';

// Codec
export {
  JsonCodec,
  ForyCodec,
  DEFAULT_COMPRESSION_THRESHOLD,
  walkTypeGraph,
  wireType,
  resolveForyConfig,
  type Codec,
  type ForyConfig,
  type ResolvedForyConfig,
} from './codec.js';

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
} from './interceptors/base.js';
export { DeadlineInterceptor } from './interceptors/deadline.js';
export { MetricsInterceptor } from './interceptors/metrics.js';
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
  TypeKind as ContractTypeKind,
  ContainerKind,
  TypeDefKind,
  MethodPattern,
  CapabilityKind,
  ScopeKind,
  type ServiceContract,
  type MethodDef as ContractMethodDef,
  type CapabilityRequirement as ContractCapabilityRequirement,
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

// Session-scoped services (extended)
export { SessionServer, SessionStub, createSession } from './session.js';

// Transport implementations
export { IrohTransport } from './transport/iroh.js';

// Configuration
export {
  configFromEnv,
  configFromFile,
  loadEndpointConfig,
  resolveRootPubkey,
  toEndpointConfig,
  loadIdentity,
  loadIdentityFile,
  findPeer,
  getProducerTokens,
  parseSimpleToml,
  printConfig,
  type AsterConfig,
  type IdentityData,
} from './config.js';

// Logging
export {
  AsterLogger,
  createLogger,
  withRequestContext,
  getRequestContext,
  type RequestContext,
  type LoggerOptions,
} from './logging.js';

// Health
export {
  HealthServer,
  ConnectionMetrics as HealthConnectionMetrics,
  AdmissionMetrics as HealthAdmissionMetrics,
  getConnectionMetrics,
  getAdmissionMetrics,
  resetMetrics,
  checkHealth,
  checkReady,
  healthStatus,
  readyStatus,
  metricsSnapshot,
  type HealthState,
  type HealthMetrics,
} from './health.js';

// High-level API
export {
  AsterServer,
  AsterClientWrapper,
  ProxyClient,
  type AsterServerOptions,
  type AsterClientOptions,
} from './high-level.js';

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
  AllowAllPolicy,
  DenyAllPolicy,
  MeshEndpointHook,
  CONSUMER_ADMISSION_ALPN,
  type ConnectionPolicy,
  type HookDecision,
} from './trust/hooks.js';
export {
  MeshState,
  saveMeshState,
  loadMeshState,
  type PeerService,
} from './trust/mesh.js';

// Producer admission
export {
  handleProducerAdmission,
  serveProducerAdmission,
  PRODUCER_ADMISSION_ALPN,
  type ProducerAdmissionRequest,
  type ProducerAdmissionResponse,
  type ProducerAdmissionOptions,
} from './trust/producer.js';

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

// Clock drift
export {
  ClockDriftTracker,
  computeDrift,
  shouldIsolate,
  DEFAULT_CLOCK_DRIFT_CONFIG,
  type ClockDriftConfig,
} from './trust/clock.js';

// Producer mesh gossip
export {
  ProducerMessageType,
  deriveGossipTopic,
  producerMessageSigningBytes,
  signProducerMessage,
  verifyProducerMessage,
  handleProducerMessage,
  encodeIntroducePayload,
  encodeDepartPayload,
  encodeContractPublishedPayload,
  encodeLeaseUpdatePayload,
  startLeaseHeartbeat,
  runLeaseHeartbeat,
  type ProducerMessage,
  type HandleMessageOptions,
} from './trust/gossip.js';

// Mesh bootstrap
export {
  startFoundingNode,
  joinMesh,
  applyAdmissionResponse,
  handleAdmissionRpc,
  handleProducerAdmissionConnection,
  makeEphemeralMeshState,
  type BootstrapConfig,
} from './trust/bootstrap.js';

// Connection & Admission Metrics (from metrics.ts)
export { ConnectionMetrics, AdmissionMetrics } from './metrics.js';

// RPC Server (QUIC accept loop)
export { RpcServer, type ServerOptions } from './server.js';

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
