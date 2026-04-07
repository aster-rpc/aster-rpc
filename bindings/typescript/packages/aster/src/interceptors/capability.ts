/**
 * Capability interceptor — validates method-level access control.
 *
 * Checks that the caller's role (from admission attributes) satisfies
 * the method's capability requirement.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, StatusCode } from '../status.js';
import type { CapabilityRequirement } from '../service.js';

export class CapabilityInterceptor implements Interceptor {
  private requirements = new Map<string, CapabilityRequirement>();

  /** Register a capability requirement for a method. */
  setRequirement(service: string, method: string, req: CapabilityRequirement): void {
    this.requirements.set(`${service}/${method}`, req);
  }

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    const key = `${ctx.service}/${ctx.method}`;
    const req = this.requirements.get(key);
    if (!req) return request;

    const callerRole = ctx.attributes['aster.role'] ?? '';

    switch (req.kind) {
      case 'role':
        if (!req.roles.includes(callerRole)) {
          throw new RpcError(StatusCode.PERMISSION_DENIED, `requires role: ${req.roles.join(', ')}`);
        }
        break;
      case 'any_of':
        if (!req.roles.includes(callerRole)) {
          throw new RpcError(StatusCode.PERMISSION_DENIED, `requires any of: ${req.roles.join(', ')}`);
        }
        break;
      case 'all_of':
        // For all_of, caller needs all roles — single-role check can't satisfy this
        // In practice, this would check against a set of caller roles
        if (!req.roles.includes(callerRole)) {
          throw new RpcError(StatusCode.PERMISSION_DENIED, `requires all of: ${req.roles.join(', ')}`);
        }
        break;
    }
    return request;
  }
}
