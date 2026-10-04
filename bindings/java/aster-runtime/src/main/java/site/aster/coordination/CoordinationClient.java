package site.aster.coordination;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.coordination.Coordination.*;
import site.aster.ffi.DataPlane;
import site.aster.handle.IrohRuntime;
import site.aster.lease.TransitionOutcome;
import site.aster.node.IrohNode;
import site.aster.upcall.Ucv1;

/**
 * A coordination client (C ABI 4.4): the durable journaled client, or a read-only one. It talks to
 * a coordination group as the identity of the {@link IrohNode} it was opened on; the group's
 * replicated ACL decides what it may do (deny by default), the client adds no credentials.
 *
 * <pre>{@code
 * try (CoordinationClient c =
 *     CoordinationClient.open(node, Options.durable(groupId, journalDir, List.of(ticket))).get()) {
 *   c.resume().get();                                  // finish a half-sent request of a past run
 *   c.unacknowledged().get().ifPresent(r -> { handle(r.reply()); r.acknowledge().get(); });
 *   try (ResolvedMutation m = c.setGrant(...).get()) {  // journaled before this returns
 *     handle(m.reply());
 *     m.acknowledge().get();                            // only now may the next mutation run
 *   }
 *   switch (c.acquireLease("jobs", key, ttlMs, 0, 0).get()) { ... }
 * }
 * }</pre>
 *
 * <p><b>Delivery contract.</b> A mutation resolves exactly once: its result is fsynced to the
 * journal before you get it and no new mutation is permitted until you {@linkplain
 * ResolvedMutation#acknowledge acknowledge} it ({@link CoordinationException.Kind#AWAITING_ACK}).
 * After a crash {@link #resume} and {@link #unacknowledged} redeliver, with the same bytes. A
 * committed typed rejection is a result ({@link MutationReply#outcomeCode}), not an exception.
 *
 * <p><b>Client ids.</b> Each journal directory is one client id, and an actor has a cap on active
 * client ids: reopen the same directory rather than creating new ones, and {@link
 * #retireOtherClient} the ids of journals you abandoned.
 *
 * <p><b>Closing is not prompt.</b> {@link #close()} returns at once, but an operation already in
 * flight (a journaled mutation may be retried by the driver for its full budget, 8 attempts with
 * backoff) completes normally and the journal's lifetime lock stays held until the last one ends;
 * an immediate reopen can fail with {@link CoordinationException.Kind#JOURNAL_LOCKED}. Handles
 * derived from a closed client are inert ({@code CLOSED}); a mutation that resolves after close is
 * not handed out, and is redelivered by {@link #unacknowledged} after reopening.
 */
public final class CoordinationClient implements AutoCloseable {

  /**
   * How to open a client. {@code journalDir} is required for the durable client and ignored by the
   * read-only one; {@code seedTickets} are Open-credential {@code aster1...} tickets, routing hints
   * only (they grant nothing).
   */
  public record Options(
      byte[] groupId, List<String> seedTickets, Path journalDir, boolean readOnly) {

    public Options {
      if (groupId == null || groupId.length != 32) {
        throw new IllegalArgumentException("groupId must be 32 bytes");
      }
      if (!readOnly && journalDir == null) {
        throw new IllegalArgumentException("a durable client needs a journalDir");
      }
    }

    public static Options durable(byte[] groupId, Path journalDir, List<String> seedTickets) {
      return new Options(groupId, seedTickets, journalDir, false);
    }

    /** A client without a journal: reads only; every journaled call is UNSUPPORTED. */
    public static Options readOnly(byte[] groupId, List<String> seedTickets) {
      return new Options(groupId, seedTickets, null, true);
    }
  }

  private final IrohNode node;
  private final IrohRuntime runtime;
  private final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();

  private CoordinationClient(IrohNode node, long handle) {
    this.node = node;
    this.runtime = node.runtime();
    this.handle = handle;
    CLEANER.register(this, new Free(runtime, handle));
  }

  /**
   * Open a client bound to {@code node}'s identity. The durable client opens (and fsyncs) its
   * journal and takes its per-actor/group lifetime lock until closed: a second open over the same
   * journal fails with {@link CoordinationException.Kind#JOURNAL_LOCKED}.
   */
  public static CompletableFuture<CoordinationClient> open(IrohNode node, Options options) {
    IrohRuntime rt = node.runtime();
    return CoordNative.submit(
        rt,
        "aster_coordination_client_open",
        (a, out) -> {
          MemorySegment o = a.allocate(CoordNative.OPTIONS_LAYOUT);
          CoordNative.check(
              (int) CoordNative.OPTIONS_INIT.invoke(o), "aster_coordination_client_options_init");
          MemorySegment group = a.allocate(32);
          group.copyFrom(MemorySegment.ofArray(options.groupId()));
          o.set(ValueLayout.JAVA_INT, 4, options.readOnly() ? CoordNative.FLAG_READ_ONLY : 0);
          o.set(ValueLayout.ADDRESS, 8, group);
          MemorySegment tickets = DataPlane.utf8List(a, options.seedTickets());
          o.asSlice(16, 16).copyFrom(tickets);
          if (!options.readOnly()) {
            o.asSlice(32, 16)
                .copyFrom(DataPlane.utf8(a, options.journalDir().toAbsolutePath().toString()));
          }
          return (int)
              CoordNative.CLIENT_OPEN.invoke(rt.nativeHandle(), node.nodeHandle(), o, 0L, out);
        },
        r -> new CoordinationClient(node, r.event().handle()));
  }

  public IrohNode node() {
    return node;
  }

  public long nativeHandle() {
    return handle;
  }

  /** The group id (32 bytes). Works on a closed client. */
  public byte[] groupId() {
    return ids()[0];
  }

  /** The local actor id (32 bytes: this node's identity as the group authenticates it). */
  public byte[] localId() {
    return ids()[1];
  }

  private byte[][] ids() {
    try (Arena a = Arena.ofConfined()) {
      MemorySegment g = a.allocate(32);
      MemorySegment l = a.allocate(32);
      CoordNative.check(
          (int) CoordNative.CLIENT_IDS.invoke(runtime.nativeHandle(), handle, g, l),
          "aster_coordination_client_ids");
      return new byte[][] {g.toArray(ValueLayout.JAVA_BYTE), l.toArray(ValueLayout.JAVA_BYTE)};
    } catch (Throwable t) {
      throw CoordNative.rethrow(t);
    }
  }

  /** The journal's current client id (16 bytes). Durable client only. */
  public CompletableFuture<byte[]> clientId() {
    return CoordNative.submit(
        runtime,
        "aster_coordination_client_id",
        (a, out) -> (int) CoordNative.CLIENT_ID.invoke(runtime.nativeHandle(), handle, 0L, out),
        r -> r.data());
  }

  // -- mutations -----------------------------------------------------------------------------

  /**
   * Run one sequenced mutation. Completes with the resolved result, already journaled; you must
   * {@link ResolvedMutation#acknowledge acknowledge} it before the next mutation. Durable client
   * only. A committed typed rejection is a result, not a failure.
   */
  public CompletableFuture<ResolvedMutation> mutate(MutationRequest request) {
    byte[] d0;
    try {
      d0 = CoordinationCodec.encodeMutationRequest(request);
    } catch (RuntimeException e) {
      return CompletableFuture.failedFuture(e);
    }
    return CoordNative.submit(
        runtime,
        "aster_coordination_mutate",
        (a, out) ->
            (int)
                CoordNative.MUTATE.invoke(
                    runtime.nativeHandle(), handle, DataPlane.bytes(a, d0), 0L, out),
        r -> resolved(r).orElseThrow(() -> new IllegalStateException("mutation without a result")));
  }

  /**
   * The saved result a previous run resolved and nobody acknowledged (empty when there is none).
   * Call it on startup, before any mutation.
   */
  public CompletableFuture<Optional<ResolvedMutation>> unacknowledged() {
    return CoordNative.submit(
        runtime,
        "aster_coordination_unacknowledged",
        (a, out) ->
            (int) CoordNative.UNACKNOWLEDGED.invoke(runtime.nativeHandle(), handle, 0L, out),
        this::resolved);
  }

  /**
   * Drive any half-finished request from a previous run to resolution; empty when the journal is
   * ready (nothing was pending).
   */
  public CompletableFuture<Optional<ResolvedMutation>> resume() {
    return CoordNative.submit(
        runtime,
        "aster_coordination_resume",
        (a, out) -> (int) CoordNative.RESUME.invoke(runtime.nativeHandle(), handle, 0L, out),
        this::resolved);
  }

  private Optional<ResolvedMutation> resolved(DataPlane.Result r) {
    long h = r.event().handle();
    if (h == 0) {
      return Optional.empty();
    }
    return Optional.of(
        new ResolvedMutation(runtime, h, CoordinationCodec.decodeMutationReply(r.data())));
  }

  public CompletableFuture<ResolvedMutation> acquire(ResourceKey resource, long ttlMs) {
    return mutate(new MutationRequest.Acquire(resource, ttlMs));
  }

  public CompletableFuture<ResolvedMutation> renew(Fence fence) {
    return mutate(new MutationRequest.Renew(fence));
  }

  public CompletableFuture<ResolvedMutation> compareAndSet(
      Fence fence, long expectedValueRevision, byte[] newValue) {
    return mutate(new MutationRequest.CompareAndSet(fence, expectedValueRevision, newValue));
  }

  public CompletableFuture<ResolvedMutation> release(Fence fence) {
    return mutate(new MutationRequest.Release(fence));
  }

  public CompletableFuture<ResolvedMutation> releaseWithValue(
      Fence fence, long expectedValueRevision, byte[] finalValue) {
    return mutate(new MutationRequest.ReleaseWithValue(fence, expectedValueRevision, finalValue));
  }

  /** {@code expectedEpoch} null = revoke whatever epoch is current. */
  public CompletableFuture<ResolvedMutation> revoke(ResourceKey resource, Long expectedEpoch) {
    return mutate(new MutationRequest.Revoke(resource, expectedEpoch));
  }

  public CompletableFuture<ResolvedMutation> setGrant(
      ResourceKey resource, byte[] targetNodeId, long rights, long expectedAclRevision) {
    return mutate(
        new MutationRequest.SetGrant(resource, targetNodeId, rights, expectedAclRevision));
  }

  public CompletableFuture<ResolvedMutation> setAdmin(
      byte[] targetNodeId, boolean enabled, long expectedAdminRevision) {
    return mutate(new MutationRequest.SetAdmin(targetNodeId, enabled, expectedAdminRevision));
  }

  public CompletableFuture<ResolvedMutation> changeMembership(
      byte[] operationId,
      LogId expectedMembershipLogId,
      List<byte[]> desiredVoters,
      List<byte[]> retainedLearners,
      List<MemberSeed> memberSeeds) {
    return mutate(
        new MutationRequest.ChangeMembership(
            operationId, expectedMembershipLogId, desiredVoters, retainedLearners, memberSeeds));
  }

  public CompletableFuture<ResolvedMutation> cancelMembership(
      byte[] operationId, int expectedPhaseCode) {
    return mutate(new MutationRequest.CancelMembership(operationId, expectedPhaseCode));
  }

  /** Retire one of this actor's <b>other</b> client ids (16 bytes). Durable client only. */
  public CompletableFuture<RetireReply> retireOtherClient(byte[] clientId) {
    if (clientId == null || clientId.length != 16) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException("clientId must be 16 bytes"));
    }
    return CoordNative.submit(
        runtime,
        "aster_coordination_retire_other_client",
        (a, out) -> {
          MemorySegment id = a.allocate(16);
          id.copyFrom(MemorySegment.ofArray(clientId));
          return (int) CoordNative.RETIRE_OTHER.invoke(runtime.nativeHandle(), handle, id, 0L, out);
        },
        r -> CoordinationCodec.decodeRetireReply(r.data()));
  }

  // -- reads (either client) -----------------------------------------------------------------

  private <T> CompletableFuture<T> read(
      ReadRequest q, java.util.function.Function<byte[], T> decode) {
    byte[] d1;
    try {
      d1 = CoordinationCodec.encodeReadRequest(q);
    } catch (RuntimeException e) {
      return CompletableFuture.failedFuture(e);
    }
    return CoordNative.submit(
        runtime,
        "aster_coordination_read",
        (a, out) ->
            (int)
                CoordNative.READ.invoke(
                    runtime.nativeHandle(), handle, DataPlane.bytes(a, d1), 0L, out),
        r -> decode.apply(r.data()));
  }

  /** The register and lease state of {@code resource}, read linearizably. */
  public CompletableFuture<ResourceSnapshot> readLinearizable(ResourceKey resource) {
    return read(new ReadRequest.ReadLinearizable(resource), CoordinationCodec::decodeReadReply);
  }

  public CompletableFuture<AclStatus> inspectAcl(ResourceKey resource) {
    return read(new ReadRequest.InspectAcl(resource), CoordinationCodec::decodeAclStatus);
  }

  public CompletableFuture<AdminStatus> inspectAdmins() {
    return read(new ReadRequest.InspectAdmins(), CoordinationCodec::decodeAdminStatus);
  }

  /** {@code operationId} null = the current operation. */
  public CompletableFuture<MembershipStatus> membershipStatus(byte[] operationId) {
    return read(
        new ReadRequest.MembershipStatusOf(operationId), CoordinationCodec::decodeMembershipStatus);
  }

  public CompletableFuture<GroupStatus> groupStatus() {
    return read(new ReadRequest.GroupStatusOf(), CoordinationCodec::decodeGroupStatus);
  }

  public CompletableFuture<ClientProbe> probeClient(byte[] targetNodeId, byte[] targetClientId) {
    return read(
        new ReadRequest.ProbeClient(targetNodeId, targetClientId),
        CoordinationCodec::decodeClientProbe);
  }

  /** Audit records after {@code after} (null = from the audit floor); {@code limit} 1..=10000. */
  public CompletableFuture<List<AuditRecord>> auditStream(LogId after, long limit) {
    return read(new ReadRequest.AuditStream(after, limit), CoordinationCodec::decodeAuditRecords);
  }

  /** The whole recovery export in memory; prefer {@link #recoveryExportToFile} for big groups. */
  public CompletableFuture<RecoveryExport> recoveryExport() {
    return read(new ReadRequest.RecoveryExportRequest(), CoordinationCodec::decodeRecoveryExport);
  }

  /**
   * Stream the recovery export into {@code path} (via {@code path.partial}, renamed into place only
   * once the checksum held).
   */
  public CompletableFuture<RecoveryManifest> recoveryExportToFile(Path path) {
    return CoordNative.submit(
        runtime,
        "aster_coordination_recovery_export_to_file",
        (a, out) ->
            (int)
                CoordNative.EXPORT_TO_FILE.invoke(
                    runtime.nativeHandle(),
                    handle,
                    DataPlane.utf8(a, path.toAbsolutePath().toString()),
                    0L,
                    out),
        r -> CoordinationCodec.decodeRecoveryManifest(r.data()));
  }

  // -- leases --------------------------------------------------------------------------------

  /** What {@link #acquireLease} found. */
  public sealed interface LeaseAcquisition {
    /** The lease is held: its fence, the renew loop and the self-limit run in Rust. */
    record Granted(CoordinationLease lease) implements LeaseAcquisition {}

    /** Someone else holds it (or the predicate refused): who, with a back-off hint. */
    record Denied(TransitionOutcome.Denied outcome) implements LeaseAcquisition {}
  }

  /**
   * One committed acquire of {@code (namespace, key)}. On a grant the Rust renew loop and
   * self-limit take over and every transition is journaled and acknowledged by the lease itself.
   * Timing {@code 0} = the defaults. Durable client only.
   */
  public CompletableFuture<LeaseAcquisition> acquireLease(
      String namespace, byte[] key, long ttlMs, long renewIntervalMs, long holderMarginMs) {
    return CoordNative.submit(
        runtime,
        "aster_coordination_lease_acquire",
        (a, out) ->
            (int)
                CoordNative.LEASE_ACQUIRE.invoke(
                    runtime.nativeHandle(),
                    handle,
                    DataPlane.utf8(a, namespace),
                    DataPlane.bytes(a, key),
                    ttlMs,
                    renewIntervalMs,
                    holderMarginMs,
                    0L,
                    out),
        r -> {
          long h = r.event().handle();
          if (h != 0) {
            return new LeaseAcquisition.Granted(
                new CoordinationLease(runtime, h, CoordinationCodec.decodeFence(r.data())));
          }
          TransitionOutcome o = Ucv1.decodeTransitionOutcome(r.data());
          if (o instanceof TransitionOutcome.Denied d) {
            return new LeaseAcquisition.Denied(d);
          }
          throw new IllegalStateException("lease acquire: unexpected outcome " + o);
        });
  }

  public CompletableFuture<LeaseAcquisition> acquireLease(
      String namespace, String key, long ttlMs, long renewIntervalMs, long holderMarginMs) {
    return acquireLease(
        namespace, key.getBytes(StandardCharsets.UTF_8), ttlMs, renewIntervalMs, holderMarginMs);
  }

  // -- lifecycle -----------------------------------------------------------------------------

  /**
   * Close (once): new calls fail with {@code CLOSED}, handles derived from this client become inert
   * (a lease stops renewing and lapses by TTL; {@link ResolvedMutation#acknowledge} fails {@code
   * CLOSED} and the result stays journaled for the next open). Returns at once; see the class docs
   * on in-flight operations and the journal lock. The native handle is freed when this object is
   * garbage collected.
   */
  @Override
  public void close() {
    if (closed.compareAndSet(false, true)) {
      try {
        CoordNative.CLIENT_CLOSE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime already closed
      }
    }
  }

  private static final java.lang.ref.Cleaner CLEANER = java.lang.ref.Cleaner.create();

  private record Free(IrohRuntime runtime, long handle) implements Runnable {
    @Override
    public void run() {
      try {
        CoordNative.CLIENT_FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime already closed, or freed
      }
    }
  }

  boolean isClosed() {
    return closed.get();
  }
}
