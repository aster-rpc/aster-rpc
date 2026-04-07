import { describe, it, expect } from 'vitest';
import {
  MAX_FRAME_SIZE,
  MAX_DECOMPRESSED_SIZE,
  MAX_METADATA_ENTRIES,
  MAX_METADATA_TOTAL_BYTES,
  HEX_FIELD_LENGTHS,
  LimitExceeded,
  validateHexField,
  validateMetadata,
  validateStatusMessage,
  MAX_STATUS_MESSAGE_LEN,
} from '@aster-rpc/aster';

describe('limit constants', () => {
  it('frame limits are 16 MiB', () => {
    expect(MAX_FRAME_SIZE).toBe(16 * 1024 * 1024);
    expect(MAX_DECOMPRESSED_SIZE).toBe(16 * 1024 * 1024);
  });

  it('hex field lengths are all even numbers', () => {
    for (const [name, len] of Object.entries(HEX_FIELD_LENGTHS)) {
      expect(len % 2).toBe(0);
    }
  });

  it('all limits are positive integers', () => {
    expect(MAX_METADATA_ENTRIES).toBeGreaterThan(0);
    expect(MAX_METADATA_TOTAL_BYTES).toBeGreaterThan(0);
    expect(MAX_STATUS_MESSAGE_LEN).toBeGreaterThan(0);
  });
});

describe('validateHexField', () => {
  it('accepts valid hex of correct length', () => {
    expect(() => validateHexField('root_pubkey', 'a'.repeat(64))).not.toThrow();
    expect(() => validateHexField('signature', 'b'.repeat(128))).not.toThrow();
  });

  it('accepts empty string (optional fields)', () => {
    expect(() => validateHexField('root_pubkey', '')).not.toThrow();
  });

  it('rejects wrong length', () => {
    expect(() => validateHexField('root_pubkey', 'aa')).toThrow(LimitExceeded);
  });

  it('rejects non-hex characters', () => {
    expect(() => validateHexField('root_pubkey', 'g'.repeat(64))).toThrow('invalid hex');
  });

  it('allows unknown field names (no length check)', () => {
    expect(() => validateHexField('custom_field', 'deadbeef')).not.toThrow();
  });
});

describe('validateMetadata', () => {
  it('accepts valid metadata', () => {
    expect(() => validateMetadata(['key'], ['value'])).not.toThrow();
  });

  it('rejects too many entries', () => {
    const keys = Array.from({ length: 65 }, (_, i) => `k${i}`);
    const values = keys.map(() => 'v');
    expect(() => validateMetadata(keys, values)).toThrow(LimitExceeded);
  });

  it('rejects oversized total bytes', () => {
    const keys = Array.from({ length: 60 }, () => 'k'.repeat(100));
    const values = Array.from({ length: 60 }, () => 'v'.repeat(100));
    // 60 * 100 + 60 * 100 = 12000 > 8192
    expect(() => validateMetadata(keys, values)).toThrow(LimitExceeded);
  });
});

describe('validateStatusMessage', () => {
  it('passes through short messages', () => {
    expect(validateStatusMessage('hello')).toBe('hello');
  });

  it('truncates long messages', () => {
    const long = 'x'.repeat(5000);
    const result = validateStatusMessage(long);
    expect(result.length).toBe(MAX_STATUS_MESSAGE_LEN);
    expect(result.endsWith('...')).toBe(true);
  });

  it('preserves messages at exact limit', () => {
    const exact = 'x'.repeat(MAX_STATUS_MESSAGE_LEN);
    expect(validateStatusMessage(exact)).toBe(exact);
  });
});

describe('LimitExceeded', () => {
  it('has field, limit, and actual properties', () => {
    const err = new LimitExceeded('test', 100, 200);
    expect(err.field).toBe('test');
    expect(err.limit).toBe(100);
    expect(err.actual).toBe(200);
    expect(err.name).toBe('LimitExceeded');
    expect(err.message).toContain('100');
    expect(err.message).toContain('200');
  });

  it('works without actual value', () => {
    const err = new LimitExceeded('test', 100);
    expect(err.actual).toBeUndefined();
    expect(err.message).not.toContain('got');
  });
});
