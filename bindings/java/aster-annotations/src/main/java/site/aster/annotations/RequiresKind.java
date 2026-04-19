package site.aster.annotations;

/**
 * Role-check kind for {@link Requires}. Mirrors the runtime {@code site.aster.contract
 * .CapabilityKind} but lives in {@code aster-annotations} so authors can attach the annotation
 * without pulling the full runtime onto their compile classpath.
 */
public enum RequiresKind {
  /** Caller must carry the single role in {@link Requires#roles()}. */
  ROLE,
  /** Caller must carry at least one of the roles in {@link Requires#roles()}. */
  ANY_OF,
  /** Caller must carry every role in {@link Requires#roles()}. */
  ALL_OF
}
