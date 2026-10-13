package site.aster.enrollment;

/** An issuer's key (64 hex) and delegation expiry (Unix seconds). */
public record IssuerInfo(String publicKey, long delegationExpiresAt) {}
