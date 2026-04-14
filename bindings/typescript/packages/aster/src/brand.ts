/**
 * Branded numeric types for explicit wire-type selection.
 *
 * TypeScript's `number` is IEEE 754 double, so the spec's language
 * mapping table (`ffi_spec/Aster-ContractIdentity.md` §11.3.2.3) maps
 * it to `float64` — the honest mapping. When a field needs a narrower
 * integer or a specific float width on the wire, annotate it with one
 * of the branded types here. The build-time scanner (`aster-gen`)
 * recognizes the brand and emits the right wire type.
 *
 * These are zero-cost at runtime: the brand is a type-level phantom.
 * The cast helpers exist so you can write `const n: i32 = i32(42)` at
 * call sites that need the branded type.
 *
 * @example
 * ```ts
 * import { WireType } from '@aster-rpc/aster';
 * import type { i32, i64, u32, f32 } from '@aster-rpc/aster';
 *
 * @WireType('inventory/Item')
 * class Item {
 *   sku = '';
 *   quantity: i32 = 0 as i32;
 *   total_bytes: i64 = 0n as i64;
 * }
 * ```
 */

/**
 * Brand key is a plain string so the scanner can detect it via
 * `checker.getPropertyOfType(t, '__asterBrand')` without needing
 * access to the same unique symbol. The `__` prefix puts the key
 * out of the way of normal user field names.
 */
export type i8  = number & { readonly __asterBrand: 'i8' };
export type i16 = number & { readonly __asterBrand: 'i16' };
export type i32 = number & { readonly __asterBrand: 'i32' };
export type i64 = bigint & { readonly __asterBrand: 'i64' };

export type u8  = number & { readonly __asterBrand: 'u8' };
export type u16 = number & { readonly __asterBrand: 'u16' };
export type u32 = number & { readonly __asterBrand: 'u32' };
export type u64 = bigint & { readonly __asterBrand: 'u64' };

export type f32 = number & { readonly __asterBrand: 'f32' };
export type f64 = number & { readonly __asterBrand: 'f64' };

export const i8  = (n: number): i8  => n as i8;
export const i16 = (n: number): i16 => n as i16;
export const i32 = (n: number): i32 => n as i32;
export const i64 = (n: bigint): i64 => n as i64;

export const u8  = (n: number): u8  => n as u8;
export const u16 = (n: number): u16 => n as u16;
export const u32 = (n: number): u32 => n as u32;
export const u64 = (n: bigint): u64 => n as u64;

export const f32 = (n: number): f32 => n as f32;
export const f64 = (n: number): f64 => n as f64;

/** Canonical wire type token for each brand, matching the §11.3.2.3 wire type set. */
export const BRAND_TO_WIRE = {
  i8:  'int8',
  i16: 'int16',
  i32: 'int32',
  i64: 'int64',
  u8:  'uint8',
  u16: 'uint16',
  u32: 'uint32',
  u64: 'uint64',
  f32: 'float32',
  f64: 'float64',
} as const;

export type AsterBrandTag = keyof typeof BRAND_TO_WIRE;
