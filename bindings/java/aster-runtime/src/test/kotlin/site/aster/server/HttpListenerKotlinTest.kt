package site.aster.server

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Test
import site.aster.server.DispatchServerE2ETest.Msg
import site.aster.upcall.Ucv1

/** Kotlin consumes the shared JVM listener and SAM authenticator without a native adapter. */
class HttpListenerKotlinTest {
    @Test
    fun publicListenerAndCertificatesFromKotlin() {
        HttpListenerE2ETest.tlsMaterial()
        val secret = ByteArray(32) { 7 }
        val cert = TlsCertificates.generateWebTransportCertificate("kotlin", listOf("127.0.0.1"), secret)
        TlsCertificates.verifyCertificateBinding(cert.certificatePem(), site.aster.id.Ids.nodeId(secret))
        HttpListenerE2ETest.Fixture().use { fixture ->
            fixture.upcalls.close()
            val config = HttpListenerConfig("127.0.0.1:0", cert.certificatePem(), cert.privateKeyPem())
            fixture.server.startPublicHttp(config).use { listener ->
                HttpListenerE2ETest.client().use { client ->
                    val reply = HttpListenerE2ETest.call(client, listener.tcpAddress(), "echo", "", fixture.body(Msg("public", 1)))
                    assertEquals(0, reply.status().code())
                    assertFalse(fixture.calls.last().irohAuthenticated())
                    assertEquals(emptyMap<String, String>(), fixture.calls.last().attributes())
                }
            }
        }
    }

    @Test
    fun authenticatedListenerFromKotlin() {
        HttpListenerE2ETest.tlsMaterial()
        HttpListenerE2ETest.Fixture().use { fixture ->
            fixture.upcalls.serveRpcAuthenticator { request, invocation ->
                assertEquals("echo", request.method())
                assertFalse(invocation.isCancelled())
                Ucv1.RpcAuthResponse(true, "kotlin/alice", mapOf("language" to "kotlin"))
            }
            fixture.server.startHttp(fixture.upcalls, HttpListenerE2ETest.config(false)).use { listener ->
                HttpListenerE2ETest.client().use { client ->
                    val reply = HttpListenerE2ETest.call(
                        client, listener.tcpAddress(), "echo", "kotlin", fixture.body(Msg("jvm", 1))
                    )
                    assertEquals(0, reply.status().code())
                    assertEquals("kotlin/alice", fixture.calls.last().peer())
                    assertEquals(mapOf("language" to "kotlin"), fixture.calls.last().attributes())
                    assertFalse(fixture.calls.last().irohAuthenticated())
                }
            }
        }
    }
}
