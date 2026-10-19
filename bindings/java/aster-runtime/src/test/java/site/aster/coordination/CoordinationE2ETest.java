package site.aster.coordination;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import site.aster.coordination.Coordination.*;
import site.aster.coordination.CoordinationClient.LeaseAcquisition;
import site.aster.coordination.Placement.Candidate;
import site.aster.coordination.Placement.PlacementPolicy;
import site.aster.lease.DenyReason;
import site.aster.lease.LeaseStatus;
import site.aster.node.IrohNode;
import site.aster.upcall.Ucv1;
import site.aster.upcall.UpcallKind;
import site.aster.upcall.Upcalls;

/**
 * The C ABI 4.4 coordination client against a real voter group in its own process (see {@link
 * VoterFixture}): open / close / journal lock, redelivery after reopen, a lease (granted, denied,
 * compare-and-set, release), retiring an earlier journal, and the placement manager over Java
 * providers. Skipped when the voter binary is not built.
 *
 * <p>These are wiring tests; reply and error decoding is in {@link CoordinationVectorsTest}. The
 * behaviour is tested once in Rust, in {@code ffi/tests/coordination.rs}: read-only clients ({@code
 * read_only_client_reads_and_refuses_the_journal}), the acknowledge rule and reads ({@code
 * durable_client_mutations_reads_and_redelivery}), ACL refusals ({@code
 * an_acl_refusal_is_permission_denied}), close with leases and operations in flight ({@code
 * close_retires_handles_and_keeps_the_delivery_contract}), concurrency ({@code
 * concurrent_reads_share_one_client}, {@code a_release_waits_for_an_in_flight_compare_and_set}) and
 * the placement manager's failure modes ({@code placement_manager_over_foreign_providers}).
 */
final class CoordinationE2ETest {

  private static final long T = 60;
  private static final long RIGHTS = 31; // read | acquire | renew | mutate | release
  private static final HexFormat HEX = HexFormat.of();

  private static VoterFixture group;
  private static IrohNode rootNode;

  @BeforeAll
  static void start() throws Exception {
    group = VoterFixture.get();
    assumeTrue(group != null, "coordination-voter is not built (see VoterFixture)");
    rootNode = VoterFixture.node(VoterFixture.rootSecret());
  }

  @AfterAll
  static void stop() {
    if (rootNode != null) {
      rootNode.close();
    }
  }

  private static <V> V get(CompletableFuture<V> f) throws Exception {
    return f.get(T, TimeUnit.SECONDS);
  }

  private static Path journal() throws Exception {
    return Files.createTempDirectory("aster-journal");
  }

  private static ResourceKey res(String key) {
    return ResourceKey.of("java-" + Long.toHexString(System.nanoTime() & 0xffffff), key);
  }

  /** Mutate and acknowledge; the reply. */
  private static MutationReply ack(CompletableFuture<ResolvedMutation> f) throws Exception {
    try (ResolvedMutation m = get(f)) {
      MutationReply reply = m.reply();
      get(m.acknowledge());
      return reply;
    }
  }

  /** ROOT grants {@code actor} every lease right on {@code r}. */
  private static void grant(CoordinationClient root, ResourceKey r, byte[] actor) throws Exception {
    AclStatus acl = get(root.inspectAcl(r));
    MutationReply reply = ack(root.setGrant(r, actor, RIGHTS, acl.aclRevision()));
    assertEquals(6, reply.outcomeCode(), "grant updated");
  }

  private static byte[] idOf(IrohNode n) {
    return HEX.parseHex(n.nodeId());
  }

  private static CoordinationClient openRoot(Path dir) throws Exception {
    return get(CoordinationClient.open(rootNode, group.durable(dir)));
  }

  // ── open / close / journal lock ──────────────────────────────────────────

  @Test
  void idsStatusAndTheJournalLock() throws Exception {
    Path dir = journal();
    try (CoordinationClient c = openRoot(dir)) {
      assertArrayEquals(group.groupId, c.groupId());
      assertArrayEquals(idOf(rootNode), c.localId());
      assertEquals(16, get(c.clientId()).length);
      GroupStatus st = get(c.groupStatus());
      assertEquals(VoterFixture.DOMAIN, st.authorityDomain());
      assertEquals(1, st.voters().size());
      assertNotNull(st.membershipLogId());

      // A second client over the same journal is refused.
      CoordinationException locked =
          assertCoordFailure(CoordinationClient.open(rootNode, group.durable(dir)));
      assertTrue(locked.isJournalLocked(), "kind " + locked.kind());
    }
    // Closed: the lock is released (retry briefly while the last op unwinds).
    try (CoordinationClient again = reopen(dir)) {
      assertEquals(16, get(again.clientId()).length);
    }
  }

  private static Path rootDir;

  /** One journal (one client id) for the disposable root clients: actors have a client cap. */
  private static synchronized Path rootDir() throws Exception {
    if (rootDir == null) {
      rootDir = journal();
    }
    return rootDir;
  }

  private static CoordinationClient reopen(Path dir) throws Exception {
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(T);
    while (true) {
      try {
        return openRoot(dir);
      } catch (ExecutionException e) {
        if (!(e.getCause() instanceof CoordinationException c)
            || !c.isJournalLocked()
            || System.nanoTime() > end) {
          throw e;
        }
        Thread.sleep(50);
      }
    }
  }

  private static CoordinationException assertCoordFailure(CompletableFuture<?> f) {
    ExecutionException e = assertThrows(ExecutionException.class, () -> f.get(T, TimeUnit.SECONDS));
    return assertInstanceOf(CoordinationException.class, e.getCause());
  }

  @Test
  void callsAfterCloseAreRefused() throws Exception {
    CoordinationClient c = openRoot(journal());
    ResourceKey closing = res("closing");
    grant(c, closing, idOf(rootNode));
    ResolvedMutation m = get(c.acquire(closing, 5_000));
    c.close();
    c.close(); // idempotent
    assertEquals(CoordinationException.Kind.CLOSED, assertCoordFailure(c.groupStatus()).kind());
    // Acknowledge on a closed client fails CLOSED and consumes the handle.
    assertEquals(CoordinationException.Kind.CLOSED, assertCoordFailure(m.acknowledge()).kind());
    assertEquals(CoordinationException.Kind.NOT_FOUND, assertCoordFailure(m.acknowledge()).kind());
    m.close(); // already consumed: a no-op
    // ids still work on a closed client's handle only until it is freed: not asserted here.
  }

  // ── mutation -> acknowledge, redelivery ──────────────────────────────────

  @Test
  void anUnacknowledgedResultIsRedeliveredAfterReopening() throws Exception {
    Path dir = journal();
    ResourceKey r = res("redeliver");
    MutationReply first;
    try (CoordinationClient root = openRoot(dir)) {
      AclStatus acl = get(root.inspectAcl(r));
      ResolvedMutation m = get(root.setGrant(r, idOf(rootNode), RIGHTS, acl.aclRevision()));
      first = m.reply();
      m.close(); // dropped without acknowledging: stays journaled
    }
    try (CoordinationClient root = reopen(dir)) {
      Optional1 again = new Optional1(get(root.unacknowledged()));
      assertTrue(again.value != null, "redelivered");
      assertArrayEquals(
          CoordinationCodec.encodeMutationReply(first),
          CoordinationCodec.encodeMutationReply(again.value.reply()));
      // Until acknowledged no new mutation is permitted.
      assertEquals(
          CoordinationException.Kind.AWAITING_ACK,
          assertCoordFailure(root.acquire(r, 5_000)).kind());
      get(again.value.acknowledge());
      assertTrue(get(root.unacknowledged()).isEmpty());
      assertTrue(get(root.resume()).isEmpty(), "journal is ready");
    }
  }

  /** A tiny holder so the test reads linearly. */
  private static final class Optional1 {
    final ResolvedMutation value;

    Optional1(java.util.Optional<ResolvedMutation> o) {
      value = o.orElse(null);
    }
  }

  // ── leases ───────────────────────────────────────────────────────────────

  @Test
  void leaseLifecycleGrantedDeniedCompareAndSetAndRelease() throws Exception {
    IrohNode a = VoterFixture.node(VoterFixture.actorSecret());
    IrohNode b = VoterFixture.node(VoterFixture.actorSecret());
    try (CoordinationClient root = reopen(rootDir());
        CoordinationClient ca = get(CoordinationClient.open(a, group.durable(journal())));
        CoordinationClient cb = get(CoordinationClient.open(b, group.durable(journal())))) {
      ResourceKey r = res("lease");
      grant(root, r, idOf(a));
      grant(root, r, idOf(b));
      grant(root, r, idOf(rootNode));

      LeaseAcquisition first = get(ca.acquireLease(r.namespace(), r.key(), 30_000, 0, 0));
      CoordinationLease lease = ((LeaseAcquisition.Granted) first).lease();
      Fence fence = lease.fence().orElseThrow();
      assertEquals(r.namespace(), fence.resource().namespace());
      assertArrayEquals(idOf(a), fence.holder());
      assertArrayEquals(group.groupId, fence.groupId());
      assertEquals(LeaseStatus.HELD, lease.status());
      assertEquals(lease.grantedFence().epoch(), fence.epoch());

      // Held by another: denied, with the holder's snapshot.
      LeaseAcquisition second = get(cb.acquireLease(r.namespace(), r.key(), 30_000, 0, 0));
      var denied = ((LeaseAcquisition.Denied) second).outcome();
      assertEquals(DenyReason.HELD_BY_OTHER, denied.reason());
      assertTrue(denied.snapshot().held());

      // Compare-and-set on the register under the lease's fence.
      MutationReply set1 = get(lease.compareAndSet(0, "v1".getBytes(StandardCharsets.UTF_8)));
      assertNotNull(set1.snapshot(), "reply: " + set1);
      ResourceSnapshot seen = get(root.readLinearizable(r));
      assertArrayEquals("v1".getBytes(StandardCharsets.UTF_8), seen.value());
      assertTrue(seen.held());
      // A stale expected revision is a typed rejection (a result), not an exception.
      MutationReply stale = get(lease.compareAndSet(0, new byte[] {1}));
      assertNotNull(stale);

      MutationReply last =
          get(
              lease.releaseWithValue(
                  seen.valueRevision(), "done".getBytes(StandardCharsets.UTF_8)));
      assertEquals(
          CoordinationException.Kind.NOT_FOUND, assertCoordFailure(lease.release()).kind());
      assertEquals(LeaseStatus.RELEASED, lease.status());
      assertTrue(lease.fence().isEmpty());
      assertNotNull(last);
      assertArrayEquals(
          "done".getBytes(StandardCharsets.UTF_8), get(root.readLinearizable(r)).value());

      // Free again: b can take it, and a graceful release works.
      LeaseAcquisition third = get(cb.acquireLease(r.namespace(), r.key(), 30_000, 0, 0));
      CoordinationLease mine = ((LeaseAcquisition.Granted) third).lease();
      assertTrue(mine.grantedFence().epoch() > fence.epoch());
      get(mine.release());
      assertFalse(get(root.readLinearizable(r)).held());
      lease.close(); // already consumed: a no-op
    } finally {
      a.close();
      b.close();
    }
  }

  // ── retire ───────────────────────────────────────────────────────────────

  @Test
  void retireOtherClientRetiresAnEarlierJournal() throws Exception {
    IrohNode a = VoterFixture.node(VoterFixture.actorSecret());
    try (CoordinationClient root = reopen(rootDir())) {
      ResourceKey r = res("retire");
      grant(root, r, idOf(a));
      byte[] oldId;
      try (CoordinationClient first = get(CoordinationClient.open(a, group.durable(journal())))) {
        oldId = get(first.clientId());
        assertTrue(ack(first.acquire(r, 5_000)).sequence() >= 1);
      }
      try (CoordinationClient second = get(CoordinationClient.open(a, group.durable(journal())))) {
        RetireReply rr = get(second.retireOtherClient(oldId));
        assertNotNull(rr);
        assertInstanceOf(
            IllegalArgumentException.class,
            assertThrows(
                    ExecutionException.class, () -> second.retireOtherClient(new byte[3]).get())
                .getCause());
      }
    } finally {
      a.close();
    }
  }

  // ── placement ────────────────────────────────────────────────────────────

  private static PlacementPolicy policy(
      GroupStatus st, List<byte[]> eligible, List<byte[]> excluded) {
    List<Candidate> cands = new ArrayList<>();
    for (byte[] id : eligible) {
      cands.add(new Candidate(id, null));
    }
    return new PlacementPolicy(
        1,
        VoterFixture.filled(0x44),
        st.authorityPublicKey(),
        st.authorityDomain(),
        1,
        0,
        cands,
        List.of(),
        excluded,
        0,
        60_000,
        false);
  }

  private static Ucv1.TopologySnapshot view(List<byte[]> ids) {
    List<String> hex = ids.stream().map(HEX::formatHex).sorted().toList();
    List<Ucv1.ClusterView> clusters = new ArrayList<>();
    List<Ucv1.FarPair> far = new ArrayList<>();
    for (int i = 0; i < hex.size(); i++) {
      clusters.add(new Ucv1.ClusterView(List.of(hex.get(i)), hex.get(i), List.of(hex.get(i))));
      for (int j = i + 1; j < hex.size(); j++) {
        far.add(new Ucv1.FarPair(hex.get(i), hex.get(j)));
      }
    }
    return new Ucv1.TopologySnapshot(clusters, far, 1);
  }

  /** A manager on its own table (one manager per table), closed together. */
  private record Managed(Upcalls table, PlacementManager manager) implements AutoCloseable {
    @Override
    public void close() {
      manager.close();
      table.close();
    }
  }

  private static Managed managed(
      CoordinationClient root,
      Placement.PolicySource policy,
      Placement.Provisioner provisioner,
      PlacementManager.TopologySource topology) {
    Upcalls up = Upcalls.create(rootNode.runtime());
    return new Managed(up, PlacementManager.create(root, up, policy, provisioner, topology));
  }

  @Test
  void placementOverJavaProviders() throws Exception {
    try (CoordinationClient root = reopen(rootDir())) {
      GroupStatus st = get(root.groupStatus());
      byte[] voter = st.voters().get(0);
      byte[] newcomer = VoterFixture.filled(0x7a);

      // 1. Roles already satisfied: policy and topology are asked, the provisioner is not.
      List<UpcallKind> asked = new java.util.concurrent.CopyOnWriteArrayList<>();
      try (Managed m =
          managed(
              root,
              () -> {
                asked.add(UpcallKind.PLACEMENT_POLICY);
                return policy(st, List.of(voter), List.of());
              },
              req -> {
                asked.add(UpcallKind.PLACEMENT_PROVISION);
                throw new IllegalStateException("not expected");
              },
              () -> {
                asked.add(UpcallKind.TOPOLOGY_SNAPSHOT);
                return view(List.of(voter));
              })) {
        PlacementReport report = get(m.manager().reconcile());
        assertEquals("current roles already satisfy policy", report.reason());
        assertEquals(1, report.policyRevision());
        assertArrayEquals(VoterFixture.filled(0x44), report.policyDigest());
        assertEquals(1, report.currentVoters().size());
        assertArrayEquals(voter, report.currentVoters().get(0));
        assertTrue(asked.contains(UpcallKind.PLACEMENT_POLICY));
        assertTrue(asked.contains(UpcallKind.TOPOLOGY_SNAPSHOT));
        assertFalse(asked.contains(UpcallKind.PLACEMENT_PROVISION));
      }

      // 2. A candidate without seed material goes to the provisioner with the idempotency
      //    tuple; its failure fails the pass.
      List<Placement.ProvisionRequest> provisioned =
          new java.util.concurrent.CopyOnWriteArrayList<>();
      try (Managed m =
          managed(
              root,
              () -> policy(st, List.of(voter, newcomer), List.of(voter)),
              req -> {
                provisioned.add(req);
                throw new IllegalStateException("no capacity in that zone");
              },
              () -> view(List.of(voter, newcomer)))) {
        CoordinationException e = assertCoordFailure(m.manager().reconcile());
        assertTrue(e.getMessage().contains("no capacity in that zone"), e.getMessage());
        assertEquals(1, provisioned.size());
        assertArrayEquals(newcomer, provisioned.get(0).nodeId());
        assertArrayEquals(VoterFixture.filled(0x44), provisioned.get(0).policyDigest());
        assertArrayEquals(group.groupId, provisioned.get(0).groupId());
      }
    }
  }
}
