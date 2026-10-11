package site.aster.upcall;

/**
 * What an upcall asks ({@code aster_upcall_kind_t}; codes never change). Decision kinds take the
 * policy's failure mode when the binding gives no answer.
 */
public enum UpcallKind {
  CANCEL(0, false),
  LEASE_TRANSITION(1, false),
  LEASE_SNAPSHOT(2, false),
  LEASE_AUTHORIZE(3, true),
  FENCED_MUTATE(4, false),
  TOPOLOGY_ADMITTED(16, true),
  TOPOLOGY_SNAPSHOT(17, false),
  PLACEMENT_POLICY(32, false),
  PLACEMENT_PROVISION(33, false),
  EXPOSE_AUTHORIZE(48, true),
  EXPOSE_HTTP(49, false),
  EXPOSE_ROUTE_POLICY(50, true),
  ADMISSION_CONNECT(64, true),
  ADMISSION_HANDSHAKE(65, true),
  RPC_AUTHENTICATE(80, false);

  public final int code;
  public final boolean decision;

  UpcallKind(int code, boolean decision) {
    this.code = code;
    this.decision = decision;
  }

  /** The kind for an ABI code, or {@code null} if unknown. */
  public static UpcallKind fromCode(int code) {
    for (UpcallKind k : values()) {
      if (k.code == code) {
        return k;
      }
    }
    return null;
  }
}
