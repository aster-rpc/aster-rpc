package site.aster.grants;

/**
 * Everything a sealed grant is bound to ({@code aster_grant_context_t}). Sealing and opening must
 * use an identical context; any difference makes the open fail.
 *
 * @param app versioned application label, e.g. {@code portal-sync/tree-grant/v1}
 * @param granter the sealing authority's node id, hex (bound verbatim)
 * @param recipient the recipient's node id, hex (bound verbatim; also the seal key)
 * @param resource opaque bytes naming what the grant unlocks
 * @param path the exact doc key the grant record is written at
 * @param role grant role, e.g. {@code read} / {@code write}
 */
public record GrantContext(
    String app, String granter, String recipient, byte[] resource, String path, String role) {

  public GrantContext {
    resource = resource == null ? new byte[0] : resource.clone();
  }

  @Override
  public byte[] resource() {
    return resource.clone();
  }

  /** The canonical associated data this context binds (see {@link Grants#aad}). */
  public byte[] aad() {
    return Grants.aad(this);
  }

  /** A copy with a different role. */
  public GrantContext withRole(String newRole) {
    return new GrantContext(app, granter, recipient, resource, path, newRole);
  }
}
