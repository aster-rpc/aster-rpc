package site.aster.codec;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import org.apache.fory.Fory;
import org.junit.jupiter.api.Test;

/**
 * CT-4: {@link ContractScalars#install} reaches into a private Fory method whose behaviour is known
 * only for the pinned release, so it refuses any other loaded fory-core.
 */
class ContractScalarsVersionTest {

  @Test
  void loadedForyIsThePinnedRelease() {
    assertEquals(ForyCodec.FORY_VERSION, Fory.class.getPackage().getImplementationVersion());
    assertDoesNotThrow(() -> ContractScalars.checkForyVersion(ForyCodec.FORY_VERSION));
  }

  @Test
  void anotherForyReleaseIsRefusedNamingBoth() {
    IllegalStateException e =
        assertThrows(IllegalStateException.class, () -> ContractScalars.checkForyVersion("1.8.0"));
    assertTrue(e.getMessage().contains("1.8.0"), e.getMessage());
    assertTrue(e.getMessage().contains(ForyCodec.FORY_VERSION), e.getMessage());
  }

  @Test
  void anUnversionedForyIsRefused() {
    IllegalStateException e =
        assertThrows(IllegalStateException.class, () -> ContractScalars.checkForyVersion(null));
    assertTrue(e.getMessage().contains("unknown version"), e.getMessage());
  }
}
