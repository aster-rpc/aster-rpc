import { WireType, ContractDefault, i32, i64 } from '@aster-rpc/aster';

/**
 * TypeScript twin of the Rust `Settings` in
 * aster/tests/contract_defaults_vector.rs; must hash to
 * conformance/vectors/contract-defaults.json.
 */
@WireType('defaults.v1/Settings')
export class Settings {
  /** required: no @ContractDefault */
  name = '';
  @ContractDefault() state = 'idle';
  @ContractDefault('') note = '';
  @ContractDefault() enabled = true;
  @ContractDefault() retries: i32 = i32(-3);
  @ContractDefault() since_ms: i64 = i64(1_700_000_000_000n);
  @ContractDefault() ratio = 0.5;
  @ContractDefault('x') label: string | null = 'x';
  @ContractDefault() tags: string[] = [];
  @ContractDefault() attrs: Map<string, string> = new Map();
  /** binary fields cannot carry a default in v1 -> required */
  blob: Uint8Array = new Uint8Array();
}
