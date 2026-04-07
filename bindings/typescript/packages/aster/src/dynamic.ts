/**
 * DynamicTypeFactory — synthesize wire-compatible types from contract manifests.
 *
 * Enables calling remote services without local type definitions.
 * Given a ContractManifest, this factory creates JavaScript classes
 * with the correct field names and Fory wire tags.
 *
 * Used by the shell, MCP server, and dynamic clients.
 */

import { WIRE_TYPE_KEY } from './decorators.js';
import type { ManifestMethod, ManifestField } from './contract/manifest.js';

/** A dynamically synthesized type class. */
export interface DynamicType {
  new (init?: Record<string, unknown>): Record<string, unknown>;
  wireTag: string;
}

/**
 * Create a dynamic type class from a wire tag and field descriptors.
 *
 * The returned class:
 * - Has the correct wire tag (set via WIRE_TYPE_KEY symbol)
 * - Accepts a partial init object in the constructor
 * - Has default values for all fields based on their type
 */
export function createDynamicType(wireTag: string, fields: ManifestField[]): DynamicType {
  // Build default values
  const defaults: Record<string, unknown> = {};
  for (const field of fields) {
    defaults[field.name] = field.default ?? defaultForType(field.type);
  }

  // Create the class dynamically
  const DynClass = class {
    static wireTag = wireTag;

    constructor(init?: Record<string, unknown>) {
      // Apply defaults
      for (const [key, value] of Object.entries(defaults)) {
        (this as any)[key] = value;
      }
      // Apply init overrides
      if (init) {
        for (const [key, value] of Object.entries(init)) {
          (this as any)[key] = value;
        }
      }
    }
  };

  // Set wire type tag
  (DynClass as any)[WIRE_TYPE_KEY] = wireTag;

  // Set the class name for debugging
  Object.defineProperty(DynClass, 'name', { value: wireTag.split('/').pop() ?? wireTag });

  return DynClass as unknown as DynamicType;
}

/**
 * Factory that synthesizes types for all methods in a manifest.
 *
 * Returns a map of wire tag -> DynamicType for both request and response types.
 */
export class DynamicTypeFactory {
  private types = new Map<string, DynamicType>();

  /**
   * Synthesize types for a method's request and response.
   * Returns [RequestType, ResponseType] or undefined if wire tags are missing.
   */
  synthesizeForMethod(method: ManifestMethod): [DynamicType, DynamicType] | undefined {
    if (!method.requestWireTag || !method.responseWireTag) return undefined;

    const reqType = this.getOrCreate(method.requestWireTag, method.fields);
    const respType = this.getOrCreate(method.responseWireTag, method.responseFields ?? []);

    return [reqType, respType];
  }

  /** Get a previously synthesized type by wire tag. */
  get(wireTag: string): DynamicType | undefined {
    return this.types.get(wireTag);
  }

  /** All synthesized types. */
  allTypes(): IterableIterator<[string, DynamicType]> {
    return this.types.entries();
  }

  private getOrCreate(wireTag: string, fields: ManifestField[]): DynamicType {
    let type = this.types.get(wireTag);
    if (!type) {
      type = createDynamicType(wireTag, fields);
      this.types.set(wireTag, type);
    }
    return type;
  }
}

/** Get default value for a field type string. */
function defaultForType(typeStr: string): unknown {
  switch (typeStr) {
    case 'str':
    case 'string':
      return '';
    case 'int':
    case 'int32':
    case 'int64':
    case 'float':
    case 'float32':
    case 'float64':
    case 'number':
      return 0;
    case 'bool':
    case 'boolean':
      return false;
    case 'bytes':
      return new Uint8Array(0);
    default:
      if (typeStr.startsWith('list[') || typeStr.startsWith('List[')) return [];
      if (typeStr.startsWith('dict[') || typeStr.startsWith('Dict[') || typeStr.startsWith('Map[')) return {};
      return null;
  }
}
