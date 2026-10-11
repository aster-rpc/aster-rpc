package site.aster.docs

import org.junit.jupiter.api.Assertions.assertArrayEquals
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import site.aster.blobs.BlobFormat
import site.aster.blobs.BlobId
import java.util.concurrent.TimeUnit

class DocContentsBatchKotlinTest {
    @Test
    fun preservesEmptyAndMissingSlotsFromKotlin() {
        val node = DocContentsBatchE2ETest.node()
        try {
            node.docs().createAsync().get(30, TimeUnit.SECONDS).use { doc ->
                node.blobs().custody().use { custody ->
                    val empty = custody.addBytesAsync(byteArrayOf(), BlobFormat.RAW).get(30, TimeUnit.SECONDS)
                    val result = doc.readEntryContentsIfCompleteAsync(listOf(empty, BlobId.of("00".repeat(32)), empty))
                        .get(30, TimeUnit.SECONDS)
                    assertEquals(3, result.size)
                    assertArrayEquals(byteArrayOf(), result[0].orElseThrow())
                    assertTrue(result[1].isEmpty)
                    assertArrayEquals(byteArrayOf(), result[2].orElseThrow())
                }
            }
        } finally {
            node.close()
            node.runtime().close()
        }
    }
}
