import { afterEach, describe, expect, it, vi } from 'vitest';

afterEach(() => {
  vi.doUnmock('node:module');
  vi.doUnmock('node:fs');
  vi.resetModules();
});

async function loader(addon: object) {
  vi.resetModules();
  vi.doMock('node:module', () => ({ createRequire: () => () => addon }));
  vi.doMock('node:fs', () => ({ existsSync: () => false }));
  return import('../../../bindings/typescript/packages/aster/src/native.js');
}

describe('native authenticated listener inventory', () => {
  it('rejects an otherwise loadable addon missing the listener export', async () => {
    const old = { Node: class {}, RpcConnection: class {}, DispatchServer: class {} };
    const { loadNative } = await loader(old);
    expect(loadNative).toThrow('HttpListener/startHttp support is required');
  });

  it('rejects an incomplete listener addon without the server start method', async () => {
    const old = { Node: class {}, RpcConnection: class {}, DispatchServer: class {}, HttpListener: class {} };
    const { tryLoadNative } = await loader(old);
    expect(tryLoadNative()).toBeNull();
  });

  it('rejects an authenticated-only addon without public listeners and TLS helpers', async () => {
    const old = { Node: class {}, RpcConnection: class {}, HttpListener: class {}, DispatchServer: class { startHttp() {} } };
    expect((await loader(old)).tryLoadNative()).toBeNull();
  });

  function compatible() {
    return {
      Node: class {}, RpcConnection: class {}, HttpListener: class {},
      DispatchServer: class { startHttp() {} startPublicHttp() {} },
      generateSelfSignedCertificate() {}, generateWebTransportCertificate() {}, verifyCertificateBinding() {},
      BlobCustody: class {}, BlobCancelToken: class {}, BlobsClient: class { custody() {} addBytesVerified() {} addPathVerified() {} },
      coordinationCanonicalGenesis() {}, coordinationCanonicalJoinPermit() {}, coordinationGenesisSigningBytes() {},
      coordinationJoinSigningBytes() {}, coordinationGenesisHash() {}, coordinationSignedDocumentFromParts() {},
      coordinationCanonicalCandidateTicket() {}, coordinationVerifyGenesis() {}, coordinationVerifyJoinPermitForStart() {},
      rpcTopologyPolicyVersion() { return 1; },
    };
  }

  it('rejects PC15 addons that would silently ignore topology policy', async () => {
    const { rpcTopologyPolicyVersion: _removed, ...old } = compatible();
    expect((await loader(old)).tryLoadNative()).toBeNull();
    expect((await loader({ ...compatible(), rpcTopologyPolicyVersion: () => 0 })).tryLoadNative()).toBeNull();
    expect((await loader({ ...compatible(), rpcTopologyPolicyVersion: () => { throw new Error("bad marker"); } })).tryLoadNative()).toBeNull();
  });

  it('retains a compatible addon', async () => {
    const current = compatible();
    const { loadNative } = await loader(current);
    expect(loadNative()).toBe(current);
  });
});
