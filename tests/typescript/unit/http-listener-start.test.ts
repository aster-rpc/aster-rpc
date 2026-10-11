import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DispatchServer } from '@aster-rpc/transport';

const table = vi.hoisted(() => ({ close: vi.fn(), uninstall: vi.fn() }));
vi.mock('../../../bindings/typescript/packages/aster/src/upcalls.js', () => ({
  Upcalls: class {
    readonly native = {};
    serveRpcAuthenticator() { return table.uninstall; }
    close() { table.close(); }
  },
}));

import { HttpListener, HttpListenerStartError } from '../../../bindings/typescript/packages/aster/src/http-listener.js';

beforeEach(() => { vi.clearAllMocks(); });

function cancelledAfterBind() {
  const controller = new AbortController();
  let calls = 0;
  const cause = new Error('startup cancelled');
  Object.defineProperty(controller.signal, 'throwIfAborted', { value: () => {
    if (++calls === 2) controller.abort(cause);
    AbortSignal.prototype.throwIfAborted.call(controller.signal);
  } });
  return { signal: controller.signal, cause };
}

describe('listener startup cleanup ownership', () => {
  it('returns a typed owner and both errors when post-bind cleanup fails, then permits retry', async () => {
    const cleanupError = new Error('stop failed');
    const stop = vi.fn().mockRejectedValueOnce(cleanupError).mockResolvedValue(undefined);
    const native = { tcpAddr: '127.0.0.1:12345', quicAddr: null, stop };
    const server = { startHttp: vi.fn().mockResolvedValue(native) } as unknown as DispatchServer;
    const { signal, cause } = cancelledAfterBind();
    let failure: HttpListenerStartError | undefined;
    try {
      await HttpListener.start(server, { certificatePem: new Uint8Array([1]), privateKeyPem: new Uint8Array([2]), signal }, () => null);
    } catch (error) {
      expect(error).toBeInstanceOf(HttpListenerStartError);
      failure = error as HttpListenerStartError;
    }
    expect(failure).toBeDefined();
    expect(failure!.cause).toBe(cause);
    expect(failure!.cleanupError).toBe(cleanupError);
    expect(failure!.listener.tcpAddr).toBe(native.tcpAddr);
    expect(table.close).not.toHaveBeenCalled();
    await failure!.listener.close();
    expect(stop).toHaveBeenCalledTimes(2);
    expect(table.uninstall).toHaveBeenCalledOnce();
    expect(table.close).toHaveBeenCalledOnce();
  });

  it('preserves a public listener owner on cleanup failure without allocating an auth table', async () => {
    const cleanupError = new Error('public stop failed');
    const stop = vi.fn().mockRejectedValueOnce(cleanupError).mockResolvedValue(undefined);
    const server = { startPublicHttp: vi.fn().mockResolvedValue({ tcpAddr: '127.0.0.1:12345', stop }) } as unknown as DispatchServer;
    const { signal, cause } = cancelledAfterBind();
    let failure: HttpListenerStartError | undefined;
    try { await HttpListener.startPublic(server, { certificatePem: new Uint8Array([1]), privateKeyPem: new Uint8Array([2]), signal }); }
    catch (error) { expect(error).toBeInstanceOf(HttpListenerStartError); failure = error as HttpListenerStartError; }
    expect(failure!.cause).toBe(cause);
    expect(failure!.cleanupError).toBe(cleanupError);
    await failure!.listener.close();
    expect(stop).toHaveBeenCalledTimes(2);
    expect(table.close).not.toHaveBeenCalled();
    expect(table.uninstall).not.toHaveBeenCalled();
  });

  it('preserves the startup cancellation when cleanup succeeds', async () => {
    const stop = vi.fn().mockResolvedValue(undefined);
    const server = { startHttp: vi.fn().mockResolvedValue({ tcpAddr: '127.0.0.1:12345', stop }) } as unknown as DispatchServer;
    const { signal, cause } = cancelledAfterBind();
    await expect(HttpListener.start(server, { certificatePem: new Uint8Array([1]), privateKeyPem: new Uint8Array([2]), signal }, () => null)).rejects.toBe(cause);
    expect(stop).toHaveBeenCalledOnce();
    expect(table.close).toHaveBeenCalledOnce();
  });
});
