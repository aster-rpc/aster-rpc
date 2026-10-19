package site.aster.server;

import com.fasterxml.jackson.core.type.TypeReference;
import java.time.Duration;
import java.util.Iterator;
import java.util.List;
import java.util.NoSuchElementException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;
import site.aster.ffi.Dispatch;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.identity.IdentityJson;

/**
 * The peers connected when {@link AsterServer#peerEvents()} was called, and the connects and
 * disconnects after them, taken together so a peer connecting during the call is in {@link
 * #snapshot()} or in the stream, never both and never neither.
 *
 * <p>Events are per peer: {@link PeerConnected} on a peer's first admitted {@code aster/1}
 * connection (past the node's gates, so a peer enrollment or a drain refuses never appears), {@link
 * PeerDisconnected} when its last one ends. Whether a connected peer serves what you need is
 * answered by reading its {@code aster.services} NodeInfo attribute.
 *
 * <p>A consumer that falls behind the bounded queue (1024 events) gets one {@link PeerLagged}; its
 * recovery is to call {@link AsterServer#peerEvents()} again. Connections are never held up by a
 * slow consumer.
 *
 * <p>{@link #next(Duration)} blocks the calling thread inside native code, so call it from a thread
 * of your own, not a tokio thread. One reader at a time. {@link #close()} (from any thread) wakes a
 * pending read; the server's {@code close()} does too.
 */
public final class PeerEvents implements AutoCloseable, Iterable<PeerEvent> {

  private static final long MAX_POLL_MS = Integer.MAX_VALUE;

  private final Dispatch dispatch;
  private final long receiver;
  private final List<PeerSnapshot> snapshot;
  private final Consumer<PeerEvents> onClose;
  private final AtomicBoolean closed = new AtomicBoolean();

  private PeerEvents(
      Dispatch dispatch, long receiver, List<PeerSnapshot> snapshot, Consumer<PeerEvents> onClose) {
    this.dispatch = dispatch;
    this.receiver = receiver;
    this.snapshot = snapshot;
    this.onClose = onClose;
  }

  /** Subscribe: {@code aster_server_peer_events}. Binding plumbing; use {@code peerEvents()}. */
  static PeerEvents open(Dispatch dispatch, Consumer<PeerEvents> onClose) {
    Dispatch.PeerEventsOpened opened = dispatch.peerEvents();
    List<PeerSnapshot> snapshot;
    try {
      snapshot =
          opened.snapshotJson().length == 0
              ? List.of()
              : List.copyOf(
                  IdentityJson.decode(
                      opened.snapshotJson(),
                      new TypeReference<List<PeerSnapshot>>() {},
                      "peer snapshot"));
    } catch (RuntimeException e) {
      dispatch.peerEventsRelease(opened.receiver());
      throw e;
    }
    return new PeerEvents(dispatch, opened.receiver(), snapshot, onClose);
  }

  /** The peers connected when the subscription was taken. */
  public List<PeerSnapshot> snapshot() {
    return snapshot;
  }

  /**
   * The next event, or {@code null} if none arrived within {@code timeout} ({@link Duration#ZERO}
   * does not wait).
   *
   * @throws IrohException {@code ALREADY_CLOSED} once this subscription is closed or the server has
   *     ended; no event will come
   */
  public PeerEvent next(Duration timeout) {
    if (timeout.isNegative()) {
      throw new IllegalArgumentException("timeout must not be negative");
    }
    if (closed.get()) {
      throw IrohException.forStatus(IrohStatus.ALREADY_CLOSED, "peer events closed");
    }
    long ms = timeout.toMillis() + (timeout.toNanosPart() % 1_000_000 != 0 ? 1 : 0);
    Dispatch.PeerPoll polled = dispatch.peerEventsPoll(receiver, (int) Math.min(ms, MAX_POLL_MS));
    return switch (polled.status()) {
      case OK -> decode(polled.json());
      case TIMEOUT -> null;
      default -> throw IrohException.forStatus(IrohStatus.ALREADY_CLOSED, "peer events closed");
    };
  }

  /**
   * Blocks until the next event; the iterator ends when this subscription is closed or the server
   * ends.
   */
  @Override
  public Iterator<PeerEvent> iterator() {
    return new Iterator<>() {
      private PeerEvent pending;
      private boolean ended;

      @Override
      public boolean hasNext() {
        while (pending == null && !ended) {
          try {
            pending = PeerEvents.this.next(Duration.ofMillis(250));
          } catch (IrohException e) {
            if (e.status() != IrohStatus.ALREADY_CLOSED) {
              throw e;
            }
            ended = true;
          }
        }
        return pending != null;
      }

      @Override
      public PeerEvent next() {
        if (!hasNext()) {
          throw new NoSuchElementException();
        }
        PeerEvent event = pending;
        pending = null;
        return event;
      }
    };
  }

  /** Releases the receiver and wakes a pending {@link #next}, which then fails or ends. */
  @Override
  public void close() {
    if (!closed.compareAndSet(false, true)) {
      return;
    }
    onClose.accept(this);
    dispatch.peerEventsRelease(receiver);
  }

  private record Wire(
      String type, String peer, String alpn, PeerIdentity identity, String reason, Long missed) {}

  private static PeerEvent decode(byte[] json) {
    Wire wire = IdentityJson.decode(json, Wire.class);
    String type = wire.type() == null ? "" : wire.type();
    return switch (type) {
      case "connected" ->
          new PeerConnected(
              wire.peer(),
              wire.alpn(),
              wire.identity() == null
                  ? new PeerIdentity(wire.peer(), null, List.of())
                  : wire.identity());
      case "disconnected" -> new PeerDisconnected(wire.peer(), reason(wire.reason()));
      case "lagged" -> new PeerLagged(wire.missed() == null ? 0 : wire.missed());
      default -> throw new IrohException("peer event: unknown type '" + type + "'");
    };
  }

  private static DisconnectReason reason(String name) {
    return switch (name == null ? "" : name) {
      case "closed" -> DisconnectReason.CLOSED;
      case "timed_out" -> DisconnectReason.TIMED_OUT;
      case "revoked" -> DisconnectReason.REVOKED;
      case "drained" -> DisconnectReason.DRAINED;
      case "error" -> DisconnectReason.ERROR;
      default -> throw new IrohException("peer event: unknown disconnect reason '" + name + "'");
    };
  }
}
