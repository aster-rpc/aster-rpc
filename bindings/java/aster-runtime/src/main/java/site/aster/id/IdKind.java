package site.aster.id;

/** The kind of a 32-byte id ({@code aster_id_kind_t}). Every kind's text form is 64 hex chars. */
public enum IdKind {
  /** A node / endpoint id: an Ed25519 public key (must be a valid curve point). */
  NODE_ID(0),
  /** An Ed25519 public key, e.g. an attestation anchor (must be a valid curve point). */
  PUBLIC_KEY(1),
  /** A node secret key (Ed25519 seed). Any 32 bytes. */
  SECRET_KEY(2),
  /** A BLAKE3 content hash. Any 32 bytes. */
  HASH(3),
  /** A docs namespace id. Any 32 bytes. */
  NAMESPACE_ID(4),
  /** A docs namespace secret (write capability). Any 32 bytes. */
  NAMESPACE_SECRET(5),
  /** A docs author id: an Ed25519 public key (must be a valid curve point). */
  AUTHOR_ID(6);

  /** The C enum value. */
  public final int code;

  IdKind(int code) {
    this.code = code;
  }
}
