package site.aster.lease;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import site.aster.contract.CapabilityKind;
import site.aster.contract.CapabilityRequirement;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.Upcalls;

/**
 * Serves {@code aster.lease.Grantor} from an {@code AsterServer} ({@code
 * AsterServer.Builder.leaseGrantor}; Rust {@code aster::rpc::lease::LeaseGrantor}; C ABI 5.1): a
 * designated serialization point other nodes acquire, renew and release leases through ({@code
 * AsterClient.leaseGrantor(addr)}). The grantor binds every operation to the caller's authenticated
 * node id, never a request field, so a node can only hold, renew or release leases as itself. A
 * dead grantor stops grants and renewals (holders run to their self-limit and stop); there is no
 * automatic failover.
 *
 * @param serializer the serialization point, implemented in Java (for example over a database) and
 *     served on an upcall table the server owns; {@code null} = Rust's in-memory reference
 *     serializer
 * @param requires gates every method (Gate 3); {@code null} = none, so admission (Gate 0, e.g.
 *     Enrollment) is the boundary. Without it any admitted caller can burn epochs (availability,
 *     never correctness)
 * @param revokeRequires gates {@code revoke}; {@code null} = the {@code operator} role
 * @param revokeUngated leave {@code revoke} gated only by {@code requires}
 */
public record LeaseGrantor(
    LeaseSerializer serializer,
    CapabilityRequirement requires,
    CapabilityRequirement revokeRequires,
    boolean revokeUngated) {

  private static final ObjectMapper MAPPER = new ObjectMapper();

  /** A grantor over Rust's in-memory serializer, revoke gated by the {@code operator} role. */
  public static LeaseGrantor inMemory() {
    return new LeaseGrantor(null, null, null, false);
  }

  /** A grantor over {@code serializer}, revoke gated by the {@code operator} role. */
  public static LeaseGrantor of(LeaseSerializer serializer) {
    return new LeaseGrantor(serializer, null, null, false);
  }

  /** A copy gating every method by one role. */
  public LeaseGrantor requiresRole(String role) {
    return new LeaseGrantor(
        serializer,
        new CapabilityRequirement(CapabilityKind.ROLE, java.util.List.of(role)),
        revokeRequires,
        revokeUngated);
  }

  /** {@code requires_json} for {@code aster_server_add_lease_grantor} (empty = none). */
  public byte[] requiresJson() {
    return requires == null ? new byte[0] : json(requires);
  }

  /** {@code revoke_requires_json} (empty = the default role, {@code null} = only requires). */
  public byte[] revokeRequiresJson() {
    if (revokeUngated) {
      return "null".getBytes(StandardCharsets.UTF_8);
    }
    return revokeRequires == null ? new byte[0] : json(revokeRequires);
  }

  private static byte[] json(CapabilityRequirement r) {
    try {
      return MAPPER.writeValueAsBytes(r);
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException(e);
    }
  }

  /**
   * The serializer this grantor runs on, started on {@code runtime}; closed by the server. Binding
   * plumbing.
   */
  public Backend start(IrohRuntime runtime) {
    if (serializer == null) {
      return new Backend(null, NativeLeaseSerializer.memory(runtime, null));
    }
    Upcalls upcalls = Upcalls.create(runtime);
    try {
      upcalls.serveLeaseSerializer(serializer);
      return new Backend(upcalls, NativeLeaseSerializer.foreign(upcalls));
    } catch (RuntimeException e) {
      upcalls.close();
      throw e;
    }
  }

  /** A running grantor serializer and the upcall table that answers it (if any). */
  public record Backend(Upcalls upcalls, NativeLeaseSerializer serializer)
      implements AutoCloseable {
    @Override
    public void close() {
      serializer.close();
      if (upcalls != null) {
        upcalls.close();
      }
    }
  }
}
