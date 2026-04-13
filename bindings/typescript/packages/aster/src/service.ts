/**
 * Service metadata types and registry.
 *
 * These types describe the structure of an Aster RPC service at runtime.
 * They are populated by the decorators in decorators.ts.
 */

import type { RpcPattern, SerializationMode } from './types.js';
import type { Metadata } from './metadata.js';

/** Capability requirement for method-level access control. */
export interface CapabilityRequirement {
  kind: 'role' | 'any_of' | 'all_of';
  roles: string[];
}

/** Method metadata describing a single RPC method. */
export interface MethodInfo {
  name: string;
  pattern: RpcPattern;
  requestType: unknown;
  responseType: unknown;
  timeout: number | undefined;
  idempotent: boolean;
  serialization: SerializationMode | undefined;
  requires: CapabilityRequirement | undefined;
  handler: ((...args: any[]) => any) | undefined;
  metadata: Metadata | undefined;
  /**
   * True if the handler declares a second parameter (interpreted as a
   * CallContext injection). Detected at @Service decoration time via
   * ``handler.length``.
   */
  acceptsCtx?: boolean;
}

/** Service metadata describing an RPC service. */
export interface ServiceInfo {
  name: string;
  version: number;
  scoped: 'shared' | 'session';
  methods: Map<string, MethodInfo>;
  serializationModes: SerializationMode[];
  requires: CapabilityRequirement | undefined;
  metadata: Metadata | undefined;
  /** The actual service instance (set when registered with a Server). */
  instance: unknown;
}

/** Attribute key used to store ServiceInfo on decorated classes. */
export const SERVICE_INFO_KEY = Symbol.for('aster.service_info');

/** Attribute key used to store MethodInfo on decorated methods. */
export const METHOD_INFO_KEY = Symbol.for('aster.method_info');

/** Get ServiceInfo from a decorated class or instance. */
export function getServiceInfo(target: unknown): ServiceInfo | undefined {
  if (target === null || target === undefined) return undefined;
  // Check on the class itself
  const info = (target as any)[SERVICE_INFO_KEY];
  if (info) return info as ServiceInfo;
  // Check on the constructor (if target is an instance)
  const ctor = (target as any).constructor;
  if (ctor) return ctor[SERVICE_INFO_KEY] as ServiceInfo | undefined;
  return undefined;
}

/**
 * Registry for looking up registered services and dispatching RPC calls.
 */
export class ServiceRegistry {
  private _services = new Map<string, ServiceInfo>(); // key: "name/version"
  private _servicesByName = new Map<string, ServiceInfo>(); // key: name (latest)

  /** Register a service instance. */
  register(serviceInstance: object): ServiceInfo {
    const info = getServiceInfo(serviceInstance);
    if (!info) {
      throw new TypeError(
        `${serviceInstance.constructor.name} is not decorated with @Service. ` +
        `Use @Service({ name: ..., version: ... }) before registering.`,
      );
    }

    const key = `${info.name}/${info.version}`;
    if (this._services.has(key)) {
      throw new Error(
        `Service ${info.name} v${info.version} is already registered.`,
      );
    }

    // Attach the instance
    info.instance = serviceInstance;

    this._services.set(key, info);
    this._servicesByName.set(info.name, info);
    return info;
  }

  /** Look up a service by name and optional version. */
  lookup(serviceName: string, version?: number): ServiceInfo | undefined {
    if (version !== undefined) {
      return this._services.get(`${serviceName}/${version}`);
    }
    return this._servicesByName.get(serviceName);
  }

  /** Look up a specific method in a service. */
  lookupMethod(
    serviceName: string,
    methodName: string,
    version?: number,
  ): [ServiceInfo, MethodInfo] | undefined {
    const svc = this.lookup(serviceName, version);
    if (!svc) return undefined;
    const method = svc.methods.get(methodName);
    if (!method) return undefined;
    return [svc, method];
  }

  /** All registered services. */
  services(): IterableIterator<ServiceInfo> {
    return this._services.values();
  }

  /** Number of registered services. */
  get size(): number {
    return this._services.size;
  }

  /** Get all registered services as an array. */
  getAllServices(): ServiceInfo[] {
    return [...this._services.values()];
  }
}

/** Look up a method on a service info object. */
export function getMethod(serviceInfo: ServiceInfo, methodName: string): MethodInfo | undefined {
  return serviceInfo.methods.get(methodName);
}

/** Check if a service has a method. */
export function hasMethod(serviceInfo: ServiceInfo, methodName: string): boolean {
  return serviceInfo.methods.has(methodName);
}

// ── Default registry singleton ────────────────────────────────────────────────

let _defaultRegistry: ServiceRegistry | undefined;

/** Get the process-wide default registry (creates one if not set). */
export function getDefaultRegistry(): ServiceRegistry {
  if (!_defaultRegistry) _defaultRegistry = new ServiceRegistry();
  return _defaultRegistry;
}

/** Set the process-wide default registry. */
export function setDefaultRegistry(registry: ServiceRegistry): void {
  _defaultRegistry = registry;
}
