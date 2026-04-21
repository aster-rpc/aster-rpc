/**
 * buildCollection unit tests.
 *
 * Exercises the contract collection layout defined in spec §11.4:
 *   contract.bin + manifest.json + types/{hex_hash}.bin (optional).
 */

import { describe, expect, it } from 'vitest';
import { buildCollection, type ContractManifest } from '@aster-rpc/aster';

const manifest: ContractManifest = {
  v: 1,
  service: 'Orders',
  version: 1,
  contractId: 'ab'.repeat(32),
  canonicalEncoding: 'fory-xlang/0.17',
  typeCount: 0,
  typeHashes: [],
  methodCount: 0,
  methods: [],
  serializationModes: ['xlang'],
  producerLanguage: '',
  scoped: 'shared',
  description: '',
  tags: [],
  deprecated: false,
};

const canonicalBytes = new Uint8Array([0x01, 0x02, 0x03]);

describe('buildCollection', () => {
  it('emits manifest.json + contract.bin when no typeDefs provided', () => {
    const entries = buildCollection(manifest, canonicalBytes);
    const names = entries.map(([n]) => n);
    expect(names).toEqual(['manifest.json', 'contract.bin']);
  });

  it('emits types/{hash}.bin entries when typeDefs provided', () => {
    const typeDefs = new Map<string, Uint8Array>([
      ['cc'.repeat(32), new Uint8Array([0xCC])],
      ['aa'.repeat(32), new Uint8Array([0xAA])],
      ['bb'.repeat(32), new Uint8Array([0xBB])],
    ]);
    const entries = buildCollection(manifest, canonicalBytes, typeDefs);

    // Base layout preserved
    expect(entries[0]![0]).toBe('manifest.json');
    expect(entries[1]![0]).toBe('contract.bin');

    // Type blobs sorted by hash (matches Python publisher)
    const typeEntries = entries.slice(2);
    expect(typeEntries.map(([n]) => n)).toEqual([
      `types/${'aa'.repeat(32)}.bin`,
      `types/${'bb'.repeat(32)}.bin`,
      `types/${'cc'.repeat(32)}.bin`,
    ]);
    expect(Array.from(typeEntries[0]![1])).toEqual([0xAA]);
    expect(Array.from(typeEntries[1]![1])).toEqual([0xBB]);
    expect(Array.from(typeEntries[2]![1])).toEqual([0xCC]);
  });

  it('empty typeDefs map behaves like undefined', () => {
    const entries = buildCollection(manifest, canonicalBytes, new Map());
    expect(entries.map(([n]) => n)).toEqual(['manifest.json', 'contract.bin']);
  });
});
