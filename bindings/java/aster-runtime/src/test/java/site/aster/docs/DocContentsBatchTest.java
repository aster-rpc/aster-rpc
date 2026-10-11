package site.aster.docs;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import org.junit.jupiter.api.Test;
import site.aster.ffi.IrohException;

final class DocContentsBatchTest {
  @Test
  void decodesMissingEmptyAndIndependentDuplicates() {
    byte[] payload =
        ByteBuffer.allocate(4 + 1 + 9 + 10 + 10)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putInt(4)
            .put((byte) 0)
            .put((byte) 1)
            .putLong(0)
            .put((byte) 1)
            .putLong(1)
            .put((byte) 7)
            .put((byte) 1)
            .putLong(1)
            .put((byte) 7)
            .array();
    var values = Doc.decodeContentsBatch(payload, 4);
    assertTrue(values.get(0).isEmpty());
    assertArrayEquals(new byte[0], values.get(1).orElseThrow());
    assertArrayEquals(new byte[] {7}, values.get(2).orElseThrow());
    values.get(2).orElseThrow()[0] = 9;
    assertArrayEquals(new byte[] {7}, values.get(3).orElseThrow());
    assertTrue(Doc.decodeContentsBatch(new byte[4], 0).isEmpty());
  }

  @Test
  void rejectsCountMismatchOversizeAndTruncationBeforeAllocation() {
    for (byte[] payload :
        new byte[][] {
          {},
          {1, 0, 0},
          {0, 0, 0, 0},
          {(byte) 0xff, (byte) 0xff, (byte) 0xff, (byte) 0xff},
          {1, 0, 0, 0},
          {1, 0, 0, 0, 1},
          {1, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, 1},
          {1, 0, 0, 0, 1, -1, -1, -1, -1, -1, -1, -1, -1},
          {2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0}
        }) assertThrows(IrohException.class, () -> Doc.decodeContentsBatch(payload, 1));
  }

  @Test
  void rejectsUnknownPresenceTrailingDataAndMissingLaterSlot() {
    assertThrows(IrohException.class, () -> Doc.decodeContentsBatch(new byte[] {1, 0, 0, 0, 2}, 1));
    assertThrows(
        IrohException.class, () -> Doc.decodeContentsBatch(new byte[] {1, 0, 0, 0, 0, 1}, 1));
    byte[] missingSlot =
        ByteBuffer.allocate(13)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putInt(2)
            .put((byte) 1)
            .putLong(0)
            .array();
    assertThrows(IrohException.class, () -> Doc.decodeContentsBatch(missingSlot, 2));
  }
}
