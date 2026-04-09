/**
 * Capability requirement helpers for method-level access control.
 *
 * @example
 * ```ts
 * import { anyOf, allOf } from '@aster-rpc/aster';
 *
 * @Rpc({ requires: anyOf('ops.status', 'ops.admin') })
 * async getStatus(req: StatusRequest): Promise<StatusResponse> { ... }
 *
 * @Rpc({ requires: allOf('ops.status', 'ops.admin') })
 * async adminStatus(req: StatusRequest): Promise<StatusResponse> { ... }
 * ```
 */

import type { CapabilityRequirement } from './service.js';

/**
 * Require ANY ONE of the listed roles (OR logic).
 * The caller is admitted if they have at least one of the specified roles.
 */
export function anyOf(...roles: string[]): CapabilityRequirement {
  return { kind: 'any_of', roles };
}

/**
 * Require ALL of the listed roles (AND logic).
 * The caller is admitted only if they have every specified role.
 */
export function allOf(...roles: string[]): CapabilityRequirement {
  return { kind: 'all_of', roles };
}
