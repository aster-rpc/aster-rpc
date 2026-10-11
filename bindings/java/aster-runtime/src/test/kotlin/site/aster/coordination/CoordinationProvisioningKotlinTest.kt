package site.aster.coordination

import org.junit.jupiter.api.Assertions.assertArrayEquals
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test

class CoordinationProvisioningKotlinTest {
    @Test
    fun canonicalAndVerifiedGenesisFromKotlin() {
        val vectors = CoordinationProvisioningTest.vectors()
        val vector = vectors.required("genesis")
        val input = CoordinationProvisioningTest.genesis(vector.required("request"))
        val document = CoordinationProvisioning.canonicalGenesis(input)
        assertArrayEquals(CoordinationProvisioningTest.hex(vector.required("document_bytes").textValue()), document)
        val verified = CoordinationProvisioning.verifyGenesis(
            CoordinationProvisioningTest.hex(vectors.required("authority_public_key").textValue()), input.authorityDomain(),
            CoordinationProvisioningTest.hex(vector.required("signed_document").textValue()))
        assertEquals(input.authorityDomain(), verified.authorityDomain())
        assertArrayEquals(document, verified.documentBytes())
    }
}
