package site.aster.config

import java.time.Duration
import org.junit.jupiter.api.Assertions.*
import org.junit.jupiter.api.Test
import site.aster.interceptors.CallContext
import site.aster.server.DrainReport
import site.aster.topology.TopologyTest

/** ABI 7.1-7.5 surfaces used from Kotlin: network / metrics config, transfer stats, context. */
class MirrorFeaturesKotlinTest {
    @Test fun networkAndMetricsSettingsFromKotlin() {
        val config = AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .discovery(AsterConfig.DISCOVERY_LOCAL)
            .advertiseAddrs(listOf("203.0.113.10:31842"))
            .metricsPeerLabels(true)
            .build()
        assertTrue(config.hasNetworkSettings())
        assertEquals("local", config.discovery())
        assertEquals(AsterConfig.METRICS_PEER_LABEL_CAP_DEFAULT, config.metricsPeerLabelCap())
    }

    @Test fun transferStatsFromKotlin() {
        val node = TopologyTest.node(false)
        node.runtime().use { node.use {
            val stats = node.blobs().transferStats()
            assertEquals(0L, stats.fetchedPayloadBytes())
            assertEquals(0L, stats.servedPayloadBytes())
        } }
    }

    @Test fun handlerContextFromKotlin() {
        val ctx = CallContext.builder("svc", "m").peer("transport").principal("alice").build()
        assertEquals("transport", ctx.peer())
        assertEquals("alice", ctx.principal())
        assertFalse(ctx.cancelled().isDone)
        val seen = CallContext.runWith(ctx) { CallContext.current().principal() }
        assertEquals("alice", seen)
        assertNull(CallContext.currentOrNull())
        assertEquals(503L, DrainReport.DRAIN_CLOSE_CODE)
        assertEquals(Duration.ZERO, DrainReport(0, 0, 0, 0, 0, 0, false, Duration.ZERO).elapsed())
    }
}
