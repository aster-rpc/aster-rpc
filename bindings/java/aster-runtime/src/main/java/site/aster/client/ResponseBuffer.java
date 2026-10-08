package site.aster.client;

import java.util.ArrayDeque;
import java.util.concurrent.TimeUnit;
import site.aster.ffi.AsterEnvelope;

/** Bounds queued plus in-flight responses before native reads or decoding. */
final class ResponseBuffer {
  static final int MAX_ITEMS = 256;
  static final long MAX_BYTES = 8L * 1024 * 1024;
  static final long MAX_RETAINED_BYTES = MAX_BYTES + AsterEnvelope.MAX_DECOMPRESSED_SIZE;

  private record Item(Object value, int bytes) {}

  private final ArrayDeque<Item> items = new ArrayDeque<>();
  private final Object end;
  private long bytes;
  private boolean reserved;
  private int pendingBytes;
  private boolean finished;
  private boolean cancelled;

  ResponseBuffer(Object end) {
    this.end = end;
  }

  synchronized void beginRead() throws InterruptedException {
    while (!cancelled && (items.size() >= MAX_ITEMS || bytes >= MAX_BYTES)) wait();
    if (cancelled) throw new InterruptedException("response reader cancelled");
    reserved = true;
  }

  synchronized void awaitCapacity(int payloadBytes) throws InterruptedException {
    if (payloadBytes > AsterEnvelope.MAX_DECOMPRESSED_SIZE)
      throw new IllegalArgumentException("response exceeds native payload limit");
    pendingBytes = payloadBytes;
    while (!cancelled && !items.isEmpty() && bytes + payloadBytes > MAX_BYTES) wait();
    if (cancelled) throw new InterruptedException("response reader cancelled");
  }

  synchronized void data(Object value, int payloadBytes) {
    releaseReservation();
    if (!cancelled) {
      items.addLast(new Item(value, payloadBytes));
      bytes += payloadBytes;
    }
    notifyAll();
  }

  synchronized void finish() {
    releaseReservation();
    finished = true;
    notifyAll();
  }

  synchronized void cancel() {
    cancelled = true;
    // Abandon buffered responses; the reader publishes the cancellation trailer.
    for (Item item : items) bytes -= item.bytes();
    items.clear();
    notifyAll();
  }

  private void releaseReservation() {
    if (reserved) {
      pendingBytes = 0;
      reserved = false;
    }
  }

  synchronized Object take() throws InterruptedException {
    while (items.isEmpty() && !finished) wait();
    return remove();
  }

  synchronized Object poll(long timeout, TimeUnit unit) throws InterruptedException {
    long remaining = unit.toNanos(timeout);
    long deadline = System.nanoTime() + remaining;
    while (items.isEmpty() && !finished) {
      if (remaining <= 0) return null;
      TimeUnit.NANOSECONDS.timedWait(this, remaining);
      remaining = deadline - System.nanoTime();
    }
    return remove();
  }

  private Object remove() {
    if (items.isEmpty()) return end;
    Item item = items.removeFirst();
    bytes -= item.bytes();
    notifyAll();
    return item.value();
  }

  synchronized int size() {
    return items.size();
  }

  synchronized int remainingCapacity() {
    return MAX_ITEMS - items.size() - (reserved ? 1 : 0);
  }

  synchronized long retainedBytes() {
    return bytes + pendingBytes;
  }
}
