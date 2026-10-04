package site.aster.grants;

/**
 * The plaintext framing published at a grant key ({@code aster_grant_record_t}, Fory). Read {@link
 * #contextVersion} before opening to choose the context; never trial-decrypt.
 *
 * @param state active or revoked
 * @param contextVersion which context template the envelope was sealed under
 * @param role the grant role (kept on revoked records for display)
 * @param envelope the sealed envelope (active) or empty (revoked)
 * @param reason the revocation reason (revoked) or empty (active)
 */
public record GrantRecord(
    State state, int contextVersion, String role, byte[] envelope, String reason) {

  /** {@code aster_grant_state_t}. */
  public enum State {
    ACTIVE,
    REVOKED
  }

  public GrantRecord {
    envelope = envelope == null ? new byte[0] : envelope.clone();
    reason = reason == null ? "" : reason;
  }

  @Override
  public byte[] envelope() {
    return envelope.clone();
  }

  /** An active record around an envelope from {@link Grants#seal}. */
  public static GrantRecord active(String role, int contextVersion, byte[] envelope) {
    return new GrantRecord(State.ACTIVE, contextVersion, role, envelope, "");
  }

  /** A revocation record. Write it at the grant key; never delete the key to revoke. */
  public static GrantRecord revoked(String role, String reason) {
    return new GrantRecord(State.REVOKED, 1, role, new byte[0], reason);
  }

  /** Whether the record grants authority now. */
  public boolean isActive() {
    return state == State.ACTIVE;
  }

  /** The Fory encoding. */
  public byte[] encode() {
    return Grants.encodeRecord(this);
  }

  /** Decode Fory grant-record bytes. */
  public static GrantRecord decode(byte[] bytes) {
    return Grants.decodeRecord(bytes);
  }
}
