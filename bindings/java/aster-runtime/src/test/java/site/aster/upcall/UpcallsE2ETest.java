package site.aster.upcall;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import site.aster.config.AsterConfig;
import site.aster.handle.IrohRuntime;
import site.aster.lease.AcquireResult;
import site.aster.lease.DenyReason;
import site.aster.lease.Fence;
import site.aster.lease.FenceRejection;
import site.aster.lease.Lease;
import site.aster.lease.LeaseOp;
import site.aster.lease.LeaseSerializer;
import site.aster.lease.LeaseSnapshot;
import site.aster.lease.LeaseStatus;
import site.aster.lease.MutateResult;
import site.aster.lease.NativeFencedResource;
import site.aster.lease.NativeLeaseSerializer;
import site.aster.lease.TransitionOutcome;
import site.aster.node.IrohNode;

/**
 * C ABI 4.1 upcalls end to end: Rust lease handles over a Java serializer (with background
 * renewals), a Java authorizer guarding Rust's memory serializer, a Java fenced resource, the
 * topology admission filter, failure modes, timeouts / cancellation and {@code UNIMPLEMENTED}.
 */
final class UpcallsE2ETest {

  private static final long T = 15;

  /** A minimal Java lease serialization point (the state machine of {@code aster::lease}). */
  static final class JavaSerializer implements LeaseSerializer {
    private static final class Row {
      long epoch;
      String holder;
      boolean held;
      long ttlMs;
      long expiresAt;
    }

    private final Map<String, Row> rows = new HashMap<>();
    final List<LeaseOp> seen = new CopyOnWriteArrayList<>();

    private static LeaseSnapshot snap(String resource, Row r, long now) {
      return new LeaseSnapshot(
          resource,
          r.epoch,
          r.held ? r.holder : null,
          r.held,
          r.held ? Math.max(0, r.expiresAt - now) : null);
    }

    @Override
    public synchronized TransitionOutcome transition(String resource, LeaseOp op) {
      seen.add(op);
      long now = System.currentTimeMillis();
      Row r = rows.computeIfAbsent(resource, k -> new Row());
      boolean live = r.held && now < r.expiresAt;
      return switch (op) {
        case LeaseOp.Acquire a -> {
          if (live) {
            yield new TransitionOutcome.Denied(DenyReason.HELD_BY_OTHER, snap(resource, r, now));
          }
          r.epoch++;
          r.holder = a.candidate();
          r.held = true;
          r.ttlMs = a.ttlMs();
          r.expiresAt = now + a.ttlMs();
          yield new TransitionOutcome.Granted(new Fence(resource, r.epoch, r.holder));
        }
        case LeaseOp.Renew n -> {
          DenyReason d = check(r, n.holder(), n.epoch());
          if (d != null) {
            yield new TransitionOutcome.Denied(d, snap(resource, r, now));
          }
          r.expiresAt = now + r.ttlMs;
          yield TransitionOutcome.RENEWED;
        }
        case LeaseOp.Release l -> {
          DenyReason d = check(r, l.holder(), l.epoch());
          if (d != null) {
            yield new TransitionOutcome.Denied(d, snap(resource, r, now));
          }
          r.epoch++;
          r.held = false;
          r.holder = null;
          yield TransitionOutcome.RELEASED;
        }
        case LeaseOp.Revoke v -> {
          r.epoch++;
          r.held = false;
          r.holder = null;
          yield TransitionOutcome.REVOKED;
        }
      };
    }

    private static DenyReason check(Row r, String holder, long epoch) {
      if (!r.held) {
        return DenyReason.NOT_HELD;
      }
      if (r.epoch != epoch) {
        return DenyReason.EPOCH_MISMATCH;
      }
      if (!holder.equals(r.holder)) {
        return DenyReason.NOT_HOLDER;
      }
      return null;
    }

    @Override
    public synchronized LeaseSnapshot snapshot(String resource) {
      return snap(
          resource, rows.computeIfAbsent(resource, k -> new Row()), System.currentTimeMillis());
    }
  }

  private static <V> V get(java.util.concurrent.CompletableFuture<V> f) throws Exception {
    return f.get(T, TimeUnit.SECONDS);
  }

  @Test
  void javaSerializerBacksRustLeaseHandles() throws Exception {
    try (IrohRuntime rt = IrohRuntime.create();
        Upcalls up = Upcalls.create(rt)) {
      JavaSerializer js = new JavaSerializer();
      up.serveLeaseSerializer(js);
      try (NativeLeaseSerializer ser = NativeLeaseSerializer.foreign(up)) {
        AcquireResult r = get(ser.acquire("tree/42", "alice", 600, 0, 0));
        Lease lease = assertInstanceOf(AcquireResult.Granted.class, r).lease();
        assertEquals(new Fence("tree/42", 1, "alice"), lease.grantedFence());
        assertEquals(1, lease.fence().orElseThrow().epoch());
        assertEquals(LeaseStatus.HELD, lease.status());

        AcquireResult bob = get(ser.acquire("tree/42", "bob", 600, 0, 0));
        var denied = assertInstanceOf(AcquireResult.Denied.class, bob);
        assertEquals(DenyReason.HELD_BY_OTHER, denied.reason());
        assertEquals("alice", denied.snapshot().holder());

        // Renewals reach Java (ttl 600 ms → every 200 ms).
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(T);
        while (js.seen.stream().filter(o -> o instanceof LeaseOp.Renew).count() < 2) {
          assertTrue(System.nanoTime() < deadline, "no renewals: " + js.seen);
          Thread.sleep(20);
        }
        assertTrue(lease.fence().isPresent());

        LeaseSnapshot snap = get(ser.snapshot("tree/42"));
        assertTrue(snap.held());
        assertEquals("alice", snap.holder());

        var watch = lease.watch();
        get(lease.release());
        assertEquals(LeaseStatus.RELEASED, get(watch));
        assertTrue(lease.fence().isEmpty());
        assertTrue(js.seen.contains(new LeaseOp.Release("alice", 1)));

        // Revoke fences the next holder's renewal.
        Lease b =
            assertInstanceOf(
                    AcquireResult.Granted.class, get(ser.acquire("tree/42", "bob", 600, 0, 0)))
                .lease();
        assertTrue(b.grantedFence().epoch() > 1);
        assertInstanceOf(TransitionOutcome.Revoked.class, get(ser.revoke("tree/42", "root")));
        assertEquals(LeaseStatus.FENCED, get(b.watch()));
        assertTrue(b.fence().isEmpty());
        b.close();
      }
    }
  }

  @Test
  void javaAuthorizerGuardsTheMemorySerializer() throws Exception {
    try (IrohRuntime rt = IrohRuntime.create();
        Upcalls up = Upcalls.create(rt)) {
      up.serveLeaseAuthorizer(op -> !op.actor().equals("mallory"));
      try (NativeLeaseSerializer ser = NativeLeaseSerializer.memory(rt, up)) {
        var m = assertInstanceOf(AcquireResult.Denied.class, get(ser.acquire("db", "mallory")));
        assertEquals(DenyReason.UNAUTHORIZED, m.reason());
        Lease a =
            assertInstanceOf(AcquireResult.Granted.class, get(ser.acquire("db", "alice"))).lease();
        a.close();
      }
      try (NativeLeaseSerializer open = NativeLeaseSerializer.memory(rt, null)) {
        assertInstanceOf(AcquireResult.Granted.class, get(open.acquire("db", "mallory")))
            .lease()
            .close();
      }
    }
  }

  @Test
  void failureModeAppliesWhenTheAuthorizerHangsOrThrows() throws Exception {
    try (IrohRuntime rt = IrohRuntime.create();
        Upcalls up = Upcalls.create(rt)) {
      CountDownLatch interrupted = new CountDownLatch(1);
      up.serveLeaseAuthorizer(
          op -> {
            if (op.actor().equals("thrower")) {
              throw new IllegalStateException("boom");
            }
            try {
              Thread.sleep(10_000);
            } catch (InterruptedException e) {
              interrupted.countDown();
              throw e;
            }
            return true;
          });
      up.setPolicy(UpcallKind.LEASE_AUTHORIZE, Duration.ofMillis(150), FailureMode.FAIL_CLOSED);
      try (NativeLeaseSerializer ser = NativeLeaseSerializer.memory(rt, up)) {
        var d = assertInstanceOf(AcquireResult.Denied.class, get(ser.acquire("r", "slow")));
        assertEquals(DenyReason.UNAUTHORIZED, d.reason());
        // Rust cancelled the timed-out upcall: the handler thread was interrupted.
        assertTrue(interrupted.await(T, TimeUnit.SECONDS));
        assertTrue(up.cancelCount() >= 1);

        assertInstanceOf(AcquireResult.Denied.class, get(ser.acquire("r", "thrower")));

        up.setPolicy(UpcallKind.LEASE_AUTHORIZE, Duration.ofMillis(150), FailureMode.FAIL_OPEN);
        assertInstanceOf(AcquireResult.Granted.class, get(ser.acquire("r", "thrower")))
            .lease()
            .close();
      }
    }
  }

  @Test
  void missingHandlerAndHandlerErrorsFailTheRustCall() throws Exception {
    try (IrohRuntime rt = IrohRuntime.create();
        Upcalls up = Upcalls.create(rt);
        NativeLeaseSerializer ser = NativeLeaseSerializer.foreign(up)) {
      var e = assertThrows(ExecutionException.class, () -> get(ser.acquire("r", "a")));
      assertTrue(e.getCause().getMessage().contains("not implemented"), e.getMessage());

      up.handle(
          UpcallKind.LEASE_TRANSITION,
          (req, dl) -> {
            throw new IllegalStateException("disk on fire");
          });
      e = assertThrows(ExecutionException.class, () -> get(ser.acquire("r", "a")));
      assertTrue(e.getCause().getMessage().contains("disk on fire"), e.getMessage());

      // Garbage in an OK answer is a decode error, never a grant.
      up.handle(UpcallKind.LEASE_TRANSITION, (req, dl) -> new byte[] {1, 2, 3});
      e = assertThrows(ExecutionException.class, () -> get(ser.acquire("r", "a")));
      assertTrue(e.getCause().getMessage().contains("malformed"), e.getMessage());
    }
  }

  @Test
  void javaFencedResource() throws Exception {
    try (IrohRuntime rt = IrohRuntime.create();
        Upcalls up = Upcalls.create(rt)) {
      // The resource's own row: epoch 7 held by alice.
      up.serveFencedResource(
          (fence, writer, op) -> {
            FenceRejection rej = FenceRejection.check(7, true, "alice", fence, writer);
            if (rej != null) {
              return new MutateResult.Rejected(rej);
            }
            return new MutateResult.Applied(
                new StringBuilder(new String(op, StandardCharsets.UTF_8))
                    .reverse()
                    .toString()
                    .getBytes(StandardCharsets.UTF_8));
          });
      try (NativeFencedResource res = NativeFencedResource.foreign(up)) {
        byte[] abc = "abc".getBytes(StandardCharsets.UTF_8);
        var ok = get(res.mutate(new Fence("tree/42", 7, "alice"), "alice", abc));
        assertArrayEquals(
            "cba".getBytes(StandardCharsets.UTF_8),
            assertInstanceOf(MutateResult.Applied.class, ok).output());
        var stale = get(res.mutate(new Fence("tree/42", 6, "alice"), "alice", abc));
        assertEquals(new MutateResult.Rejected(FenceRejection.staleEpoch(6, 7)), stale);
        var imp = get(res.mutate(new Fence("tree/42", 7, "alice"), "mallory", abc));
        assertEquals(new MutateResult.Rejected(FenceRejection.notHolder()), imp);
      }
    }
  }

  @Test
  void admittedFilterIsBatchedAndCached() throws Exception {
    try (IrohRuntime rt = IrohRuntime.create();
        Upcalls up = Upcalls.create(rt)) {
      List<List<String>> batches = new CopyOnWriteArrayList<>();
      up.serveAdmittedFilter(
          ids -> {
            batches.add(ids);
            List<Boolean> out = new ArrayList<>();
            for (String id : ids) {
              out.add(id.startsWith("a"));
            }
            return out;
          });
      String a = "a".repeat(64);
      String b = "b".repeat(64);
      String c = "c".repeat(64);
      try (AdmittedFilterHandle f = AdmittedFilterHandle.create(up)) {
        get(f.prefetch(List.of(a, b)));
        assertEquals(List.of(List.of(a, b)), batches);
        assertTrue(f.isAdmitted(a));
        assertFalse(f.isAdmitted(b));
        assertFalse(f.isAdmitted(c)); // miss: fail-closed answer, resolved in the background
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(T);
        while (batches.size() < 2) {
          assertTrue(System.nanoTime() < deadline);
          Thread.sleep(10);
        }
        f.set(b, true);
        assertTrue(f.isAdmitted(b));
        f.clear();
        AdmittedFilterHandle.Stats s = f.stats();
        assertEquals(3, s.hits());
        assertEquals(1, s.misses());
        assertTrue(s.upcalls() >= 2);
        assertThrows(RuntimeException.class, () -> f.isAdmitted("zz"));
      }
    }
  }

  @Test
  void enableSharedInstallsTheJavaFilter() throws Exception {
    AsterConfig cfg =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .enableMonitoring(true)
            .build();
    IrohNode node = IrohNode.start(cfg).get(30, TimeUnit.SECONDS);
    try (Upcalls up = Upcalls.create(node.runtime())) {
      AtomicInteger calls = new AtomicInteger();
      up.serveAdmittedFilter(
          ids -> {
            calls.incrementAndGet();
            return ids.stream().map(id -> true).toList();
          });
      try (AdmittedFilterHandle f = AdmittedFilterHandle.create(up)) {
        byte[] secret = new byte[32];
        java.util.Arrays.fill(secret, (byte) 0x42);
        assertNull(get(Topology.enableShared(node, secret, f, List.of(), Duration.ofSeconds(1))));
        assertNotNull(f.stats());
      }
    } finally {
      node.close();
    }
  }

  private static void assertNull(Object o) {
    org.junit.jupiter.api.Assertions.assertNull(o);
  }

  @Test
  void closeFailsPendingUpcalls() throws Exception {
    IrohRuntime rt = IrohRuntime.create();
    try {
      Upcalls up = Upcalls.create(rt);
      CountDownLatch entered = new CountDownLatch(1);
      up.handle(
          UpcallKind.LEASE_TRANSITION,
          (req, dl) -> {
            entered.countDown();
            Thread.sleep(10_000);
            return new byte[0];
          });
      NativeLeaseSerializer ser = NativeLeaseSerializer.foreign(up);
      var pending = ser.acquire("r", "a");
      assertTrue(entered.await(T, TimeUnit.SECONDS));
      up.close();
      assertThrows(ExecutionException.class, () -> get(pending));
      ser.close();
    } finally {
      rt.close();
    }
  }
}
