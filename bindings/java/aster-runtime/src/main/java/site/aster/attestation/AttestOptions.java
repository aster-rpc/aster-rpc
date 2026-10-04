package site.aster.attestation;

/**
 * Policy of a minted attestation edge ({@code aster_attest_options_t}).
 *
 * @param epoch monotonic replay counter (caller-managed; 0 for a first issue)
 * @param notBefore 0 = no lower bound; else unix seconds before which the chain is invalid
 * @param notAfter 0 = no expiry; else unix seconds after which the chain is invalid
 */
public record AttestOptions(long epoch, long notBefore, long notAfter) {
  /** Epoch 0, no time bounds. */
  public static final AttestOptions DEFAULT = new AttestOptions(0, 0, 0);
}
