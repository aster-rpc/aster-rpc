/**
 * Capability roles for Mission Control (Chapter 5).
 *
 * Each role maps to a string that appears in the aster.role attribute
 * of enrollment credentials. Values match the Python example exactly.
 */

export const Role = {
  STATUS: "ops.status",
  LOGS: "ops.logs",
  ADMIN: "ops.admin",
  INGEST: "ops.ingest",
} as const;

export type RoleValue = typeof Role[keyof typeof Role];
