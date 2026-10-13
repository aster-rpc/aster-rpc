package site.aster.lease;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import site.aster.client.AsterClient;
import site.aster.config.AsterConfig;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * {@code aster.lease.Grantor} through {@link AsterServer.Builder#leaseGrantor} and {@link
 * AsterClient#leaseGrantor} (C ABI 5.1), mirroring Go's {@code lease_grantor_test.go}: the grantor
 * binds each operation to the caller's authenticated node id.
 */
@Timeout(value = 90)
final class LeaseGrantorE2ETest {

  static AsterConfig loopback() {
    return AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build();
  }

  @Test
  void memoryGrantorBindsHoldersToTheirNodeId() throws Exception {
    try (AsterServer server =
            AsterServer.builder()
                .config(loopback())
                .leaseGrantor(LeaseGrantor.inMemory())
                .build()
                .get(30, TimeUnit.SECONDS);
        AsterClient a = AsterClient.builder().config(loopback()).build().get(30, TimeUnit.SECONDS);
        AsterClient b =
            AsterClient.builder().config(loopback()).build().get(30, TimeUnit.SECONDS)) {
      NodeAddr addr = server.node().nodeAddr();
      try (NativeLeaseSerializer serA = a.leaseGrantor(addr).get(30, TimeUnit.SECONDS);
          NativeLeaseSerializer serB = b.leaseGrantor(addr).get(30, TimeUnit.SECONDS)) {
        AcquireResult first =
            serA.acquire("db/primary", a.nodeId(), 3000, 0, 0).get(30, TimeUnit.SECONDS);
        Lease lease = assertInstanceOf(AcquireResult.Granted.class, first).lease();
        try {
          assertEquals(a.nodeId(), lease.grantedFence().holder());
          assertEquals("db/primary", lease.grantedFence().resource());
          assertInstanceOf(
              AcquireResult.Denied.class,
              serB.acquire("db/primary", b.nodeId(), 3000, 0, 0).get(30, TimeUnit.SECONDS),
              "b while a holds");
          // b acquiring as a fails locally: not a denial.
          assertThrows(
              ExecutionException.class,
              () -> serB.acquire("db/other", a.nodeId()).get(30, TimeUnit.SECONDS));
          // Revoke needs the operator role by default.
          assertThrows(
              ExecutionException.class,
              () -> serA.revoke("db/primary", a.nodeId()).get(30, TimeUnit.SECONDS));
          lease.release().get(30, TimeUnit.SECONDS);
        } finally {
          lease.close();
        }
        AcquireResult second =
            serB.acquire("db/primary", b.nodeId(), 3000, 0, 0).get(30, TimeUnit.SECONDS);
        try (Lease leaseB = assertInstanceOf(AcquireResult.Granted.class, second).lease()) {
          LeaseSnapshot snap =
              server.leaseSerializer().snapshot("db/primary").get(30, TimeUnit.SECONDS);
          assertTrue(snap.held());
          assertEquals(b.nodeId(), snap.holder());
          assertEquals(b.nodeId(), leaseB.grantedFence().holder());
        }
      }
    }
  }
}
