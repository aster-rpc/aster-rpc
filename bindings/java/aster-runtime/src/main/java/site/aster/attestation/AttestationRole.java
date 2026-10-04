package site.aster.attestation;

/** An identity's role within a verified chain ({@code aster_attestation_role_t}). */
public enum AttestationRole {
  /** The leaf node the chain binds. */
  NODE,
  /** The trusted anchor the chain terminates at. */
  ANCHOR,
  /** An intermediate delegating identity between node and anchor. */
  INTERMEDIATE
}
