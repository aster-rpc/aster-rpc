package site.aster.event;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohLibrary;

/**
 * Immutable view over a native {@code iroh_event_t} struct.
 *
 * <p>Field mapping (matches Rust iroh_event_t layout exactly):
 *
 * <ul>
 *   <li>{@code handle} — primary handle (endpoint, connection, or stream depending on event kind)
 *   <li>{@code related} — secondary handle (e.g., recv stream for {@code STREAM_OPENED})
 *   <li>{@code user_data} — echoed from the FFI call that initiated the operation
 * </ul>
 */
public record IrohEvent(
    IrohEventKind kind,
    long operation,
    long handle,
    long related,
    long userData,
    int status,
    int errorCode,
    int flags,
    long buffer,
    long dataLen,
    MemorySegment data) {

  public static IrohEvent fromSegment(MemorySegment seg) {
    long dataPtr = seg.get(ValueLayout.ADDRESS, 48).address();
    long dataLen = seg.get(ValueLayout.JAVA_LONG, 56);
    Arena sharedArena = IrohLibrary.getInstance().sharedArena();
    MemorySegment data =
        dataPtr != 0
            ? MemorySegment.ofAddress(dataPtr).reinterpret(dataLen, sharedArena, null)
            : MemorySegment.NULL;

    return new IrohEvent(
        IrohEventKind.fromCode((int) seg.get(ValueLayout.JAVA_INT, 4)),
        seg.get(ValueLayout.JAVA_LONG, 16),
        seg.get(ValueLayout.JAVA_LONG, 24),
        seg.get(ValueLayout.JAVA_LONG, 32),
        seg.get(ValueLayout.JAVA_LONG, 40),
        (int) seg.get(ValueLayout.JAVA_INT, 8),
        (int) seg.get(ValueLayout.JAVA_INT, 72),
        (int) seg.get(ValueLayout.JAVA_INT, 76),
        seg.get(ValueLayout.JAVA_LONG, 64),
        dataLen,
        data);
  }

  /**
   * True if this event carries a native buffer that must be released via {@code
   * iroh_buffer_release}.
   */
  public boolean hasBuffer() {
    return buffer != 0;
  }

  public String toString() {
    return "IrohEvent{kind=%s, op=%d, handle=%d, related=%d, userData=%d, status=%d, errorCode=%d}"
        .formatted(kind, operation, handle, related, userData, status, errorCode);
  }
}
