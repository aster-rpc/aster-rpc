package site.aster.blobs

import org.junit.jupiter.api.Assertions.assertArrayEquals
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
import site.aster.tags.TagFormat
import java.util.concurrent.TimeUnit

/** Kotlin uses the same owned JVM scopes and cooperative operation API. */
class BlobCustodyKotlinTest {
    @Test
    fun retainNamedRootFromKotlin() {
        val node = BlobCustodyE2ETest.node()
        try {
            val data = "kotlin custody".toByteArray()
            val hash = node.blobs().custody().use { scope ->
                val hash = scope.addBytesAsync(data, BlobFormat.RAW).get(30, TimeUnit.SECONDS)
                assertArrayEquals(data, scope.readAsync(hash).get(30, TimeUnit.SECONDS))
                scope.retainAsync(hash, BlobFormat.RAW, "kotlin/root").get(30, TimeUnit.SECONDS)
                hash
            }
            node.blobs().gcRunOnceAsync().get(30, TimeUnit.SECONDS)
            val tag = node.tags().getAsync("kotlin/root").get(30, TimeUnit.SECONDS)
            assertEquals(hash, tag.hash())
            assertEquals(TagFormat.RAW, tag.format())
        } finally {
            node.close()
            node.runtime().close()
        }
    }
}
