import { describe, it, expect } from 'vitest';
import {
  StatusCode,
  statusName,
  RpcError,
  CancelledError,
  NotFoundError,
  InvalidArgumentError,
  DeadlineExceededError,
  PermissionDeniedError,
  InternalError,
  UnavailableError,
  UnauthenticatedError,
} from '@aster-rpc/aster';

describe('StatusCode', () => {
  it('has correct numeric values (gRPC-compatible)', () => {
    expect(StatusCode.OK).toBe(0);
    expect(StatusCode.CANCELLED).toBe(1);
    expect(StatusCode.UNKNOWN).toBe(2);
    expect(StatusCode.INVALID_ARGUMENT).toBe(3);
    expect(StatusCode.DEADLINE_EXCEEDED).toBe(4);
    expect(StatusCode.NOT_FOUND).toBe(5);
    expect(StatusCode.ALREADY_EXISTS).toBe(6);
    expect(StatusCode.PERMISSION_DENIED).toBe(7);
    expect(StatusCode.RESOURCE_EXHAUSTED).toBe(8);
    expect(StatusCode.FAILED_PRECONDITION).toBe(9);
    expect(StatusCode.ABORTED).toBe(10);
    expect(StatusCode.OUT_OF_RANGE).toBe(11);
    expect(StatusCode.UNIMPLEMENTED).toBe(12);
    expect(StatusCode.INTERNAL).toBe(13);
    expect(StatusCode.UNAVAILABLE).toBe(14);
    expect(StatusCode.DATA_LOSS).toBe(15);
    expect(StatusCode.UNAUTHENTICATED).toBe(16);
  });

  it('statusName returns human-readable names', () => {
    expect(statusName(StatusCode.OK)).toBe('OK');
    expect(statusName(StatusCode.NOT_FOUND)).toBe('NOT_FOUND');
    expect(statusName(StatusCode.INTERNAL)).toBe('INTERNAL');
  });
});

describe('RpcError', () => {
  it('constructs with code and message', () => {
    const err = new RpcError(StatusCode.NOT_FOUND, 'resource missing');
    expect(err.code).toBe(StatusCode.NOT_FOUND);
    expect(err.message).toContain('NOT_FOUND');
    expect(err.message).toContain('resource missing');
    expect(err.details).toEqual({});
  });

  it('constructs with details', () => {
    const err = new RpcError(StatusCode.INTERNAL, 'oops', { key: 'value' });
    expect(err.details).toEqual({ key: 'value' });
  });

  it('is instanceof Error', () => {
    const err = new RpcError(StatusCode.OK);
    expect(err).toBeInstanceOf(Error);
    expect(err).toBeInstanceOf(RpcError);
  });
});

describe('RpcError.fromStatus', () => {
  it('returns specific subclass for known codes', () => {
    expect(RpcError.fromStatus(StatusCode.CANCELLED)).toBeInstanceOf(CancelledError);
    expect(RpcError.fromStatus(StatusCode.NOT_FOUND)).toBeInstanceOf(NotFoundError);
    expect(RpcError.fromStatus(StatusCode.INVALID_ARGUMENT)).toBeInstanceOf(InvalidArgumentError);
    expect(RpcError.fromStatus(StatusCode.DEADLINE_EXCEEDED)).toBeInstanceOf(DeadlineExceededError);
    expect(RpcError.fromStatus(StatusCode.PERMISSION_DENIED)).toBeInstanceOf(PermissionDeniedError);
    expect(RpcError.fromStatus(StatusCode.INTERNAL)).toBeInstanceOf(InternalError);
    expect(RpcError.fromStatus(StatusCode.UNAVAILABLE)).toBeInstanceOf(UnavailableError);
    expect(RpcError.fromStatus(StatusCode.UNAUTHENTICATED)).toBeInstanceOf(UnauthenticatedError);
  });

  it('returns base RpcError for OK (no specific subclass)', () => {
    const err = RpcError.fromStatus(StatusCode.OK);
    expect(err).toBeInstanceOf(RpcError);
    expect(err.code).toBe(StatusCode.OK);
  });

  it('passes message and details through', () => {
    const err = RpcError.fromStatus(StatusCode.NOT_FOUND, 'gone', { id: '123' });
    expect(err).toBeInstanceOf(NotFoundError);
    expect(err.message).toContain('gone');
    expect(err.details).toEqual({ id: '123' });
  });
});

describe('typed error subclasses', () => {
  it('each has correct code and name', () => {
    const err = new NotFoundError('missing');
    expect(err.code).toBe(StatusCode.NOT_FOUND);
    expect(err.name).toBe('NotFoundError');
    expect(err).toBeInstanceOf(RpcError);
    expect(err).toBeInstanceOf(NotFoundError);
  });
});
