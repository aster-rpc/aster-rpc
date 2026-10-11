import type { BlobsClient as NativeBlobsClient, BlobCustody as NativeCustody, BlobCancelToken, FetchReport } from '@aster-rpc/transport';
import type { BlobFormat, FetchStrategy } from './data-plane.js';
import { loadNative } from './native.js';

export interface BlobOperationOptions { signal?: AbortSignal; }
export type BlobCustodyErrorKind = 'closed' | 'cancelled' | 'hash_mismatch' | 'unprotected_root' | 'native';
export class BlobCustodyError extends Error {
  constructor(readonly kind: BlobCustodyErrorKind, cause: unknown) {
    super(cause instanceof Error ? cause.message : String(cause), { cause }); this.name = 'BlobCustodyError';
  }
}
function failure(error: unknown): unknown {
  const message = error instanceof Error ? error.message : String(error);
  const kinds: Record<string, BlobCustodyErrorKind> = { ASTER_BLOB_CLOSED: 'closed', ASTER_BLOB_CANCELLED: 'cancelled', ASTER_BLOB_HASH_MISMATCH: 'hash_mismatch', ASTER_BLOB_UNPROTECTED_ROOT: 'unprotected_root', ASTER_BLOB_ERROR: 'native' };
  const kind = kinds[message.split(':', 1)[0]!];
  return kind ? new BlobCustodyError(kind, error) : error;
}
async function operation<T>(options: BlobOperationOptions | undefined, work: (token: BlobCancelToken) => Promise<T>): Promise<T> {
  options?.signal?.throwIfAborted();
  const token = new (loadNative().BlobCancelToken)();
  const cancel = () => token.cancel();
  options?.signal?.addEventListener('abort', cancel, { once: true });
  try {
    // No Promise.race: native cleanup/commit owns the terminal outcome.
    return await work(token);
  } catch (error) { throw failure(error); }
  finally { options?.signal?.removeEventListener('abort', cancel); }
}

/** Temporary GC custody without persistent anonymous tags. Close seals admission, not accepted work. */
export class BlobCustody implements Disposable {
  /** @internal */ constructor(private readonly native: NativeCustody) {}
  protect(hash: string, format: BlobFormat = 'raw', options?: BlobOperationOptions): Promise<void> {
    return operation(options, token => this.native.protect(hash, format, token));
  }
  addBytes(data: Uint8Array, format: BlobFormat = 'raw', options?: BlobOperationOptions): Promise<string> {
    return operation(options, token => this.native.addBytes(Buffer.from(data), format, token));
  }
  /** Copy file bytes; never retain a mutable source reference. */
  addPath(path: string, format: BlobFormat = 'raw', options?: BlobOperationOptions): Promise<string> {
    return operation(options, token => this.native.addPath(path, format, token));
  }
  /** Replace a named root held with exactly this format. Adds no fsync guarantee. */
  retain(hash: string, format: BlobFormat, tag: string, options?: BlobOperationOptions): Promise<void> {
    return operation(options, token => this.native.retain(hash, format, tag, token));
  }
  /** Read root bytes without interpreting HashSeq as a Collection. */
  read(hash: string, options?: BlobOperationOptions): Promise<Uint8Array> {
    return operation(options, token => this.native.read(hash, token));
  }
  /** Fetch to the store under custody; cancellation can wait for stalled native transfers. */
  downloadHashFrom(hash: string, providers: string[], format: BlobFormat = 'raw', strategy: FetchStrategy = 'whole', options?: BlobOperationOptions): Promise<FetchReport> {
    return operation(options, token => this.native.downloadHashFrom(hash, providers, format, strategy, token));
  }
  close(): void { this.native.close(); }
  [Symbol.dispose](): void { this.close(); }
}

export type BlobsClient = Omit<NativeBlobsClient, 'custody' | 'addBytesVerified' | 'addPathVerified'> & {
  custody(): BlobCustody;
  addBytesVerified(data: Uint8Array, expectedHash: string, format: BlobFormat, tag: string, options?: BlobOperationOptions): Promise<string>;
  addPathVerified(path: string, expectedHash: string, format: BlobFormat, tag: string, options?: BlobOperationOptions): Promise<string>;
};

/** @internal Keep native methods bound to their NAPI receiver while adding cooperative host APIs. */
export function wrapBlobsClient(native: NativeBlobsClient): BlobsClient {
  const methods = {
    custody: () => new BlobCustody(native.custody()),
    addBytesVerified: (data: Uint8Array, expected: string, format: BlobFormat, tag: string, options?: BlobOperationOptions) => operation(options, token => native.addBytesVerified(Buffer.from(data), expected, format, tag, token)),
    addPathVerified: (path: string, expected: string, format: BlobFormat, tag: string, options?: BlobOperationOptions) => operation(options, token => native.addPathVerified(path, expected, format, tag, token)),
  };
  return new Proxy(native, { get(target, property) {
    if (Object.hasOwn(methods, property)) return methods[property as keyof typeof methods];
    const value = Reflect.get(target, property, target); return typeof value === 'function' ? value.bind(target) : value;
  } }) as unknown as BlobsClient;
}
