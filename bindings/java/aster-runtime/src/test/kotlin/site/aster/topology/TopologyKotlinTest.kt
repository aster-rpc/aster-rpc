package site.aster.topology

import java.util.concurrent.TimeUnit
import org.junit.jupiter.api.Assertions.*
import org.junit.jupiter.api.Test

class TopologyKotlinTest {
    @Test fun nativeSnapshotOwnerIsUsableFromKotlin() {
        val node = TopologyTest.node(true)
        node.runtime().use { node.use {
            val topology = node.topology()
            assertTrue(topology.hasMonitoring())
            topology.enableSharedAsync(ByteArray(32), null).get(10, TimeUnit.SECONDS)
            topology.snapshotAsync().get(5, TimeUnit.SECONDS).use { snapshot ->
                assertNotNull(snapshot.clusters())
                assertEquals(Topology.SeparationVerdict.UNKNOWN,
                    snapshot.separated(node.nodeId(), "00".repeat(32)))
            }
        } }
    }
}
