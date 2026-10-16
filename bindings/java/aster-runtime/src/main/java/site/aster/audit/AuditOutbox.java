package site.aster.audit;

import com.fasterxml.jackson.databind.JsonNode;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.ref.Cleaner;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;
import java.util.function.Function;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.UpcallNative;

/**
 * Explicit durable audit submission through the shared Rust engine. Submission success means local
 * durability; delivery is at least once. Canceling a future does not retract accepted work and may
 * leave an event committed without a returned ID. Close preserves pending records.
 */
public final class AuditOutbox implements AutoCloseable {
  private static final Cleaner CLEANER = Cleaner.create();
  private static final BigInteger U64_MAX = BigInteger.ONE.shiftLeft(64).subtract(BigInteger.ONE);

  /** Receiver identity and credentials are separate. Diagnostic rendering excludes both. */
  public record Destination(String endpoint, String receiverIdentity, String authorization) {
    public Destination {
      Objects.requireNonNull(endpoint, "endpoint");
    }

    public Destination(String endpoint) {
      this(endpoint, null, null);
    }

    @Override
    public String toString() {
      return "AuditDestination{redacted}";
    }
  }

  /**
   * Limits include retained dead-letter evidence and receipts. {@code acknowledgementLogMaxEntries}
   * and {@code acknowledgementLogMaxAgeMs} bound the acknowledgement log (ABI 7.1); {@code null}
   * keeps the Rust defaults (100,000 entries, 7 days).
   */
  public record Config(
      Path directory,
      Destination destination,
      BigInteger maxRetainedBytes,
      int maxEventBytes,
      Long acknowledgementLogMaxEntries,
      Long acknowledgementLogMaxAgeMs) {
    public Config {
      Objects.requireNonNull(directory, "directory");
      Objects.requireNonNull(destination, "destination");
      unsigned(maxRetainedBytes);
      if (maxEventBytes < 0)
        throw new IllegalArgumentException("maxEventBytes must be nonnegative");
      if (acknowledgementLogMaxEntries != null && acknowledgementLogMaxEntries < 0)
        throw new IllegalArgumentException("acknowledgementLogMaxEntries must be nonnegative");
      if (acknowledgementLogMaxAgeMs != null && acknowledgementLogMaxAgeMs < 0)
        throw new IllegalArgumentException("acknowledgementLogMaxAgeMs must be nonnegative");
    }

    public Config(
        Path directory, Destination destination, BigInteger maxRetainedBytes, int maxEventBytes) {
      this(directory, destination, maxRetainedBytes, maxEventBytes, null, null);
    }

    /** A copy with the acknowledgement-log bounds set ({@code null} = the Rust default). */
    public Config withAcknowledgementLog(Long maxEntries, Long maxAgeMs) {
      return new Config(
          directory, destination, maxRetainedBytes, maxEventBytes, maxEntries, maxAgeMs);
    }

    public Config(Path directory, Destination destination) {
      this(directory, destination, BigInteger.valueOf(256L * 1024 * 1024), 64 * 1024);
    }
  }

  /** Exact unsigned timestamp, kind and structured JSON. Payload is defensively copied. */
  public record Event(BigInteger timestampMs, String kind, JsonNode payload) {
    public Event {
      unsigned(timestampMs);
      Objects.requireNonNull(kind, "kind");
      payload = Objects.requireNonNull(payload, "payload").deepCopy();
      AuditJson.validatePayload(payload);
    }

    public Event(long timestampMs, String kind, JsonNode payload) {
      this(BigInteger.valueOf(timestampMs), kind, payload);
    }

    @Override
    public JsonNode payload() {
      return payload.deepCopy();
    }

    @Override
    public String toString() {
      return "AuditEvent{redacted}";
    }
  }

  public record DeliveryError(
      String kind,
      Integer httpStatus,
      BigInteger sequence,
      boolean immediateFlushFailure,
      String message) {}

  public record Status(
      String outboxId,
      boolean storageRecovered,
      BigInteger committedSequence,
      BigInteger pendingRecords,
      BigInteger pendingBytes,
      BigInteger deadLetterRecords,
      BigInteger deadLetterBytes,
      BigInteger dispositionReceipts,
      BigInteger dispositionBytes,
      BigInteger retainedBytes,
      BigInteger maxRetainedBytes,
      BigInteger queuedRecords,
      BigInteger queuedBytes,
      BigInteger oldestAgeMs,
      boolean accepting,
      boolean ready,
      String deliveryState,
      DeliveryError lastDeliveryError,
      boolean storageWorkerAlive,
      boolean deliveryWorkerAlive,
      boolean maintenance,
      BigInteger maintenanceDurationMs,
      boolean closed,
      BigInteger durableCommits,
      BigInteger lastGroupSize) {}

  public record DispositionReceipt(
      String eventId,
      String action,
      BigInteger timestampMs,
      String reason,
      String operatorReference,
      String destinationFingerprint,
      DeliveryError rejection) {}

  /** Original immutable bytes are returned as base64, alongside retained disposition receipts. */
  public record QuarantineExport(
      String eventId, String envelopeBase64, boolean pending, List<DispositionReceipt> receipts) {
    public QuarantineExport {
      receipts = List.copyOf(receipts);
    }
  }

  /**
   * One event the receiver acknowledged as durably stored (ABI 7.1). The outbox writes it in the
   * same local transaction that deletes the event, so an event is either still pending or present
   * here until the log's count / age bounds prune it. {@code cursor} increases with each
   * acknowledgement and is never reused: pass the last one seen as {@code after} to continue.
   * {@code batchId} is the digest the receiver echoed; {@code acknowledgedAtMs} is local Unix
   * milliseconds when the acknowledgement committed.
   */
  public record Acknowledgement(
      long cursor, String eventId, long sequence, String batchId, long acknowledgedAtMs) {
    public Acknowledgement {
      Objects.requireNonNull(eventId, "eventId");
      Objects.requireNonNull(batchId, "batchId");
    }

    public Instant acknowledgedAt() {
      return Instant.ofEpochMilli(acknowledgedAtMs);
    }
  }

  /** The largest page one acknowledgement read returns. */
  public static final int ACKNOWLEDGEMENT_PAGE_MAX = 1000;

  /** Mirrors {@code ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS}: one hour. */
  public static final Duration ACKNOWLEDGEMENT_MAX_WAIT = Duration.ofMillis(3_600_000L);

  private static final Duration WATCH_WAIT = Duration.ofSeconds(30);

  private final IrohRuntime runtime;
  private final long handle;
  private final Cleaner.Cleanable cleanable;
  private boolean closed;
  private CompletableFuture<Void> closing;

  private AuditOutbox(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
    this.cleanable = CLEANER.register(this, new Free(runtime, handle));
  }

  private static final class Free implements Runnable {
    private final IrohRuntime runtime;
    private final long handle;
    private final AtomicBoolean freed = new AtomicBoolean();

    Free(IrohRuntime runtime, long handle) {
      this.runtime = runtime;
      this.handle = handle;
    }

    @Override
    public void run() {
      if (freed.compareAndSet(false, true)) AuditNative.free(runtime, handle);
    }
  }

  private static void unsigned(BigInteger value) {
    if (value == null || value.signum() < 0 || value.compareTo(U64_MAX) > 0) {
      throw new IllegalArgumentException("expected an unsigned 64-bit integer");
    }
  }

  /** Canceling open's returned future releases any native handle that completes later. */
  public static CompletableFuture<AuditOutbox> open(IrohRuntime runtime, Config config) {
    byte[] json;
    try {
      json = AuditJson.config(config);
    } catch (RuntimeException error) {
      return CompletableFuture.failedFuture(error);
    }
    CompletableFuture<AuditOutbox> output = new CompletableFuture<>();
    AuditNative.submit(
            runtime,
            "open",
            (arena, out) ->
                (int)
                    AuditNative.OPEN.invoke(
                        runtime.nativeHandle(), DataPlane.bytes(arena, json), 0L, out))
        .whenComplete(
            (result, failure) -> {
              if (failure != null) {
                output.completeExceptionally(failure);
                return;
              }
              try {
                if (result.event().kind() != IrohEventKind.AUDIT_OUTBOX_OPENED
                    || result.event().handle() == 0) {
                  throw new AuditOutboxException(
                      AuditOutboxException.Kind.OTHER, "invalid audit open completion");
                }
                AuditOutbox outbox = new AuditOutbox(runtime, result.event().handle());
                if (!output.complete(outbox)) outbox.close();
              } catch (RuntimeException error) {
                output.completeExceptionally(error);
              }
            });
    return output;
  }

  /** Clone one native handle; workers, lifecycle and capacity remain shared. */
  public synchronized AuditOutbox copy() {
    if (closed)
      throw new AuditOutboxException(AuditOutboxException.Kind.CLOSED, "audit handle is closed");
    try {
      long cloned =
          UpcallNative.callForLong(
              "aster_audit_outbox_clone",
              (arena, out) -> (int) AuditNative.CLONE.invoke(runtime.nativeHandle(), handle, out));
      return new AuditOutbox(runtime, cloned);
    } catch (IrohException error) {
      throw AuditOutboxException.from(error);
    }
  }

  private synchronized <T> CompletableFuture<T> call(
      String name,
      byte[] json,
      MethodHandle operation,
      IrohEventKind expected,
      Function<DataPlane.Result, T> decode) {
    if (closed)
      return CompletableFuture.failedFuture(
          new AuditOutboxException(AuditOutboxException.Kind.CLOSED, "audit handle is closed"));
    return AuditNative.submit(
            runtime,
            name,
            (arena, out) ->
                json == null
                    ? (int) operation.invoke(runtime.nativeHandle(), handle, 0L, out)
                    : (int)
                        operation.invoke(
                            runtime.nativeHandle(), handle, DataPlane.bytes(arena, json), 0L, out))
        .thenApply(
            result -> {
              if (result.event().kind() != expected)
                throw new AuditOutboxException(
                    AuditOutboxException.Kind.OTHER, "unexpected audit completion");
              return decode.apply(result);
            });
  }

  /** Returns the full stable ID after an immediate-durability local commit. */
  public CompletableFuture<String> submit(Event event) {
    try {
      return call(
          "submit",
          AuditJson.request("event", event),
          AuditNative.SUBMIT,
          IrohEventKind.AUDIT_OUTBOX_SUBMITTED,
          result -> new String(result.data(), StandardCharsets.UTF_8));
    } catch (RuntimeException error) {
      return CompletableFuture.failedFuture(error);
    }
  }

  public CompletableFuture<Status> status() {
    return call(
        "status",
        null,
        AuditNative.STATUS,
        IrohEventKind.AUDIT_OUTBOX_STATUS,
        result -> AuditJson.response(result.data(), "status", Status.class));
  }

  /** Wait for the committed prefix. Timeout never deletes retained records. */
  public synchronized CompletableFuture<Void> flush(Duration timeout) {
    if (closed)
      return CompletableFuture.failedFuture(
          new AuditOutboxException(AuditOutboxException.Kind.CLOSED, "audit handle is closed"));
    long millis;
    try {
      if (timeout.isNegative()) throw new ArithmeticException("negative timeout");
      millis = timeout.toMillis();
    } catch (ArithmeticException error) {
      return CompletableFuture.failedFuture(
          new AuditOutboxException(
              AuditOutboxException.Kind.INVALID_CONFIG, "invalid flush timeout"));
    }
    return AuditNative.submit(
            runtime,
            "flush",
            (arena, out) ->
                (int) AuditNative.FLUSH.invoke(runtime.nativeHandle(), handle, millis, 0L, out))
        .thenApply(AuditOutbox::unitResult);
  }

  public CompletableFuture<Void> reconfigure(Destination destination) {
    return unit("reconfigure", "destination", destination, AuditNative.RECONFIGURE);
  }

  public CompletableFuture<Void> quarantine(
      String eventId, String reason, String operatorReference) {
    return unit(
        "quarantine",
        "quarantine",
        Map.of("event_id", eventId, "reason", reason, "operator_reference", operatorReference),
        AuditNative.QUARANTINE);
  }

  public CompletableFuture<QuarantineExport> export(String eventId) {
    try {
      return call(
          "export",
          AuditJson.request("event_id", eventId),
          AuditNative.EXPORT,
          IrohEventKind.AUDIT_OUTBOX_EXPORTED,
          result -> AuditJson.response(result.data(), "export", QuarantineExport.class));
    } catch (RuntimeException error) {
      return CompletableFuture.failedFuture(error);
    }
  }

  public CompletableFuture<Void> replay(String eventId) {
    return unit("replay", "event_id", eventId, AuditNative.REPLAY);
  }

  /**
   * Up to {@code limit} (1..{@link #ACKNOWLEDGEMENT_PAGE_MAX}) logged acknowledgements with a
   * cursor greater than {@code after}, oldest first. Pass 0 to start at the oldest retained entry.
   */
  public CompletableFuture<List<Acknowledgement>> acknowledgements(long after, int limit) {
    return waitAcknowledgements(after, limit, Duration.ZERO);
  }

  /**
   * {@link #acknowledgements}, but when nothing follows {@code after} the read waits up to {@code
   * wait} (at most {@link #ACKNOWLEDGEMENT_MAX_WAIT}) for the next acknowledgement, then completes
   * with an empty page. Cancelling the future abandons the wait.
   */
  public CompletableFuture<List<Acknowledgement>> waitAcknowledgements(
      long after, int limit, Duration wait) {
    if (limit < 1 || limit > ACKNOWLEDGEMENT_PAGE_MAX)
      return invalidAck("acknowledgement limit must be between 1 and 1000");
    if (after < 0) return invalidAck("acknowledgement cursor must be nonnegative");
    if (wait == null || wait.isNegative() || wait.compareTo(ACKNOWLEDGEMENT_MAX_WAIT) > 0)
      return invalidAck("acknowledgement wait must be between 0 and one hour");
    byte[] json;
    try {
      json =
          AuditJson.MAPPER.writeValueAsBytes(
              Map.of("version", 1, "after", after, "limit", limit, "wait_ms", wait.toMillis()));
    } catch (IOException error) {
      return invalidAck("invalid acknowledgement request");
    }
    return call(
        "acknowledgements",
        json,
        AuditNative.ACKNOWLEDGEMENTS,
        IrohEventKind.AUDIT_OUTBOX_ACKNOWLEDGEMENTS,
        result -> AuditJson.acknowledgements(result.data()));
  }

  private static <T> CompletableFuture<T> invalidAck(String message) {
    return CompletableFuture.failedFuture(
        new AuditOutboxException(AuditOutboxException.Kind.INVALID_EVENT, message));
  }

  /**
   * Streams acknowledgements with a cursor greater than {@code after} to {@code consumer}, in
   * order, until the returned handle is closed or the outbox fails. Delivery is lossless while
   * entries are retained: record the last {@link Acknowledgement#cursor()} to resume after a
   * restart. {@link Watch#done()} completes normally after {@link Watch#close()} and exceptionally
   * with the failure otherwise.
   */
  public Watch watchAcknowledgements(long after, Consumer<Acknowledgement> consumer) {
    Objects.requireNonNull(consumer, "consumer");
    Watch watch = new Watch();
    watch.next(this, after, consumer);
    return watch;
  }

  /** A running {@link #watchAcknowledgements} loop. */
  public static final class Watch implements AutoCloseable {
    private final CompletableFuture<Void> done = new CompletableFuture<>();
    private volatile boolean stopped;
    private volatile CompletableFuture<?> pending;

    private Watch() {}

    private void next(AuditOutbox outbox, long after, Consumer<Acknowledgement> consumer) {
      if (stopped) {
        done.complete(null);
        return;
      }
      CompletableFuture<List<Acknowledgement>> page =
          outbox.waitAcknowledgements(after, ACKNOWLEDGEMENT_PAGE_MAX, WATCH_WAIT);
      pending = page;
      page.whenComplete(
          (acks, failure) -> {
            if (stopped) {
              done.complete(null);
              return;
            }
            if (failure != null) {
              done.completeExceptionally(
                  failure instanceof CompletionException ? failure.getCause() : failure);
              return;
            }
            long cursor = after;
            try {
              for (Acknowledgement ack : acks) {
                if (stopped) break;
                consumer.accept(ack);
                cursor = ack.cursor();
              }
            } catch (RuntimeException error) {
              done.completeExceptionally(error);
              return;
            }
            next(outbox, cursor, consumer);
          });
    }

    /** Completes when the loop stops. */
    public CompletableFuture<Void> done() {
      return done;
    }

    /** Stop the loop; a waiting read is abandoned. */
    @Override
    public void close() {
      stopped = true;
      CompletableFuture<?> current = pending;
      if (current != null) current.cancel(true);
      done.complete(null);
    }
  }

  private static Void unitResult(DataPlane.Result result) {
    if (result.event().kind() != IrohEventKind.UNIT_RESULT)
      throw new AuditOutboxException(
          AuditOutboxException.Kind.OTHER, "unexpected audit completion");
    return null;
  }

  private CompletableFuture<Void> unit(String name, String key, Object value, MethodHandle method) {
    try {
      return call(
          name, AuditJson.request(key, value), method, IrohEventKind.UNIT_RESULT, result -> null);
    } catch (RuntimeException error) {
      return CompletableFuture.failedFuture(error);
    }
  }

  /** Await shutdown and directory-lock release. Canceling this wait does not stop cleanup. */
  public synchronized CompletableFuture<Void> closeAsync() {
    if (closing == null) {
      closed = true;
      closing =
          AuditNative.submit(
                  runtime,
                  "close",
                  (arena, out) ->
                      (int) AuditNative.CLOSE.invoke(runtime.nativeHandle(), handle, 0L, out))
              .handle(
                  (result, failure) -> {
                    cleanable.clean();
                    if (failure != null)
                      throw new java.util.concurrent.CompletionException(failure);
                    return unitResult(result);
                  });
    }
    return closing.thenApply(value -> null);
  }

  /** Enqueue shared worker shutdown and return immediately; use closeAsync to await release. */
  @Override
  public void close() {
    closeAsync();
  }
}
