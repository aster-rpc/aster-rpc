/**
 * RCAN (Role-based Capability Access control Network) validation.
 *
 * Spec reference: Aster-trust-spec.md
 *
 * Evaluates caller roles against capability requirements (ROLE, ANY_OF, ALL_OF).
 * Also provides encode/decode for RCAN grant bytes (opaque until spec pins format).
 */

import type { CapabilityRequirement } from '../service.js';

/** Extract caller roles from attributes (comma-separated aster.role). */
export function extractCallerRoles(attributes: Record<string, string>): Set<string> {
  const roleStr = attributes['aster.role'] ?? '';
  if (!roleStr) return new Set();
  return new Set(roleStr.split(',').map(r => r.trim()).filter(Boolean));
}

/**
 * Evaluate a capability requirement against caller attributes.
 *
 * @returns true if the caller satisfies the requirement.
 */
export function evaluateCapability(
  requirement: CapabilityRequirement,
  callerAttributes: Record<string, string>,
): boolean {
  const roles = extractCallerRoles(callerAttributes);

  switch (requirement.kind) {
    case 'role':
      // Must have the single required role
      return requirement.roles.length > 0 && roles.has(requirement.roles[0]!);
    case 'any_of':
      // Must have at least one of the listed roles
      return requirement.roles.some(r => roles.has(r));
    case 'all_of':
      // Must have every listed role
      return requirement.roles.every(r => roles.has(r));
    default:
      return false;
  }
}

/**
 * Validate an RCAN grant (opaque bytes).
 * Currently a stub — non-empty grants accepted, empty bytes rejected.
 *
 * @returns [valid, reason]
 */
export function validateRcan(rcanBytes: Uint8Array): [boolean, string | undefined] {
  if (rcanBytes.length === 0) {
    return [false, 'empty RCAN grant'];
  }
  return [true, undefined];
}

/** Pass-through encoder for RCAN grant bytes. */
export function encodeRcan(data: Uint8Array): Uint8Array {
  return data;
}

/** Pass-through decoder for RCAN grant bytes. */
export function decodeRcan(data: Uint8Array): Uint8Array {
  return data;
}
