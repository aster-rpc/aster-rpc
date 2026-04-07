/**
 * IID (Instance Identity Document) — cloud identity verification.
 *
 * Spec reference: Aster-trust-spec.md
 *
 * Validates that a connecting peer is running on an expected cloud instance
 * (AWS/GCP/Azure) with specific attributes (account, region, role ARN).
 *
 * IID verification is Gate 2 (runtime check) in the admission pipeline.
 * Triggered when credential attributes carry aster.iid_provider.
 */

/** Reserved IID attribute keys. */
export const ATTR_IID_PROVIDER = 'aster.iid_provider';
export const ATTR_IID_ACCOUNT = 'aster.iid_account';
export const ATTR_IID_REGION = 'aster.iid_region';
export const ATTR_IID_ROLE_ARN = 'aster.iid_role_arn';

/** IID verification backend interface. */
export interface IIDBackend {
  verify(
    attributes: Record<string, string>,
    iidToken?: string,
  ): Promise<[boolean, string | undefined]>;
}

/**
 * Mock IID backend for testing.
 */
export class MockIIDBackend implements IIDBackend {
  private shouldPass: boolean;
  private reason: string | undefined;
  private expectedAttributes: Record<string, string> | undefined;

  constructor(opts?: {
    shouldPass?: boolean;
    reason?: string;
    expectedAttributes?: Record<string, string>;
  }) {
    this.shouldPass = opts?.shouldPass ?? true;
    this.reason = opts?.reason;
    this.expectedAttributes = opts?.expectedAttributes;
  }

  async verify(
    attributes: Record<string, string>,
    _iidToken?: string,
  ): Promise<[boolean, string | undefined]> {
    if (this.expectedAttributes) {
      for (const [key, expected] of Object.entries(this.expectedAttributes)) {
        if (attributes[key] !== expected) {
          return [false, `attribute mismatch: ${key}`];
        }
      }
    }
    return [this.shouldPass, this.reason];
  }
}

/**
 * AWS IID backend (stub — full RSA verification deferred to production).
 */
export class AWSIIDBackend implements IIDBackend {
  async verify(
    attributes: Record<string, string>,
    _iidToken?: string,
  ): Promise<[boolean, string | undefined]> {
    // Stub: check that required attributes are present
    if (!attributes[ATTR_IID_ACCOUNT]) {
      return [false, 'missing aster.iid_account'];
    }
    if (!attributes[ATTR_IID_REGION]) {
      return [false, 'missing aster.iid_region'];
    }
    // Full verification would fetch instance identity from metadata service
    return [true, undefined];
  }
}

/**
 * GCP IID backend (stub).
 */
export class GCPIIDBackend implements IIDBackend {
  async verify(
    _attributes: Record<string, string>,
    _iidToken?: string,
  ): Promise<[boolean, string | undefined]> {
    return [true, undefined];
  }
}

/**
 * Azure IID backend (stub).
 */
export class AzureIIDBackend implements IIDBackend {
  async verify(
    _attributes: Record<string, string>,
    _iidToken?: string,
  ): Promise<[boolean, string | undefined]> {
    return [true, undefined];
  }
}

/**
 * Factory: get the appropriate IID backend for a provider.
 */
export function getIIDBackend(provider: string): IIDBackend {
  switch (provider.toLowerCase()) {
    case 'aws':
      return new AWSIIDBackend();
    case 'gcp':
      return new GCPIIDBackend();
    case 'azure':
      return new AzureIIDBackend();
    case 'mock':
      return new MockIIDBackend();
    default:
      throw new Error(`unknown IID provider: ${provider}`);
  }
}

/**
 * Run IID verification against credential attributes.
 *
 * If no backend supplied, auto-selects based on aster.iid_provider attribute.
 *
 * @returns [ok, reason]
 */
export async function verifyIID(
  attributes: Record<string, string>,
  backend?: IIDBackend,
  iidToken?: string,
): Promise<[boolean, string | undefined]> {
  const provider = attributes[ATTR_IID_PROVIDER];
  if (!provider) {
    return [true, undefined]; // No IID required
  }

  const iidBackend = backend ?? getIIDBackend(provider);
  return iidBackend.verify(attributes, iidToken);
}
