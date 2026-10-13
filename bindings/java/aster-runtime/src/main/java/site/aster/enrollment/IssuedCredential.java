package site.aster.enrollment;

/**
 * A credential an issuer signed. Hand {@code chain} to the subject: it is public (possession
 * without the subject's key is useless).
 */
public record IssuedCredential(String chain, String credentialId, long expiresAt) {}
