package site.aster.identity

import org.junit.jupiter.api.Assertions.*
import org.junit.jupiter.api.Test
import site.aster.client.CallOptions
import site.aster.config.AsterConfig
import site.aster.interceptors.CallContext
import site.aster.interceptors.RpcError
import site.aster.interceptors.StatusCode

/** ABI 7.7 gateway identity surfaces used from Kotlin: config, call options, handler context. */
class GatewayIdentityKotlinTest {
    @Test fun identityConfigFromKotlin() {
        val config = AsterConfig.builder()
            .acceptOnBehalfOf(true)
            .trustedIssuer(TrustedIssuer.of("https://idp.test", listOf("notes"))
                .withRoles("groups", mapOf("readers" to "notes.read")))
            .remoteConfigLock(listOf("identity"))
            .build()
        assertTrue(config.hasIdentitySettings())
        assertEquals("groups", config.trustedIssuers().single().rolesClaim())
    }

    @Test fun onBehalfOfFromKotlin() {
        val o = CallOptions.DEFAULT.withOnBehalfOf("jwt")
        assertEquals("jwt", o.metadata()[GatewayIdentity.ON_BEHALF_OF_METADATA])
        assertEquals("jwt", GatewayIdentity.bearerToken("Bearer jwt").orElseThrow())
    }

    @Test fun handlerContextFromKotlin() {
        val ctx = CallContext.builder("svc", "m")
            .peer("gw").principal("iss|alice").actor("gw")
            .onBehalfOf(ForwardedIdentity("iss", "alice", "", null, listOf(GatewayIdentity.AMR_WEBAUTHN)))
            .build()
        val seen = CallContext.runWith(ctx) {
            CallContext.current().requireAuthMethod(GatewayIdentity.AMR_WEBAUTHN)
            CallContext.current().actor()
        }
        assertEquals("gw", seen)
        val e = assertThrows(RpcError::class.java) { ctx.requireAuthMethod(GatewayIdentity.AMR_PASSWORD) }
        assertEquals(StatusCode.PERMISSION_DENIED, e.code())
        assertEquals(NodeAttribute.ofInt(3).asInt().orElseThrow(), 3L)
    }
}
