package computer.iroh.bridge;

import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Objects;
import java.util.concurrent.*;

/**
 * Async-first Java FFM binding layer for the Rust iroh bridge.
 */
public final class IrohNative implements AutoCloseable {
    public static final int IROH_STATUS_OK = 0;
    public static final int IROH_STATUS_BUFFER_TOO_SMALL = 5;

    public static final int IROH_RELAY_MODE_DEFAULT = 0;
    public static final int IROH_RELAY_MODE_CUSTOM = 1;
    public static final int IROH_RELAY_MODE_DISABLED = 2;

    public static final int IROH_EVENT_ENDPOINT_CREATED = 1;
    public static final int IROH_EVENT_ENDPOINT_CREATE_FAILED = 2;
    public static final int IROH_EVENT_CONNECT_SUCCEEDED = 3;
    public static final int IROH_EVENT_CONNECT_FAILED = 4;
    public static final int IROH_EVENT_INCOMING_CONNECTION = 5;
    public static final int IROH_EVENT_STREAM_OPEN_SUCCEEDED = 6;
    public static final int IROH_EVENT_STREAM_OPEN_FAILED = 7;
    public static final int IROH_EVENT_FRAME_RECEIVED = 8;
    public static final int IROH_EVENT_SEND_COMPLETED = 9;
    public static final int IROH_EVENT_STREAM_FINISHED = 10;
    public static final int IROH_EVENT_STREAM_RESET = 11;
    public static final int IROH_EVENT_OPERATION_CANCELLED = 12;
    public static final int IROH_EVENT_ERROR = 13;

    private static final Linker LINKER = Linker.nativeLinker();

    private static final GroupLayout RUNTIME_CONFIG = MemoryLayout.structLayout(
            ValueLayout.JAVA_INT.withName("event_queue_capacity"),
            ValueLayout.JAVA_INT.withName("reserved")
    );

    private static final GroupLayout ENDPOINT_CONFIG = MemoryLayout.structLayout(
            ValueLayout.ADDRESS.withName("secret_key_ptr"),
            ValueLayout.JAVA_LONG.withName("secret_key_len"),
            ValueLayout.ADDRESS.withName("relay_url_ptrs"),
            ValueLayout.JAVA_LONG.withName("relay_url_count"),
            ValueLayout.JAVA_INT.withName("relay_mode"),
            ValueLayout.JAVA_INT.withName("enable_default_discovery"),
            ValueLayout.JAVA_INT.withName("reserved0"),
            ValueLayout.JAVA_INT.withName("_pad0"),
            ValueLayout.JAVA_LONG.withName("reserved1")
    );

    private static final GroupLayout CONNECT_REQUEST = MemoryLayout.structLayout(
            ValueLayout.ADDRESS.withName("remote_addr_ptr"),
            ValueLayout.JAVA_LONG.withName("remote_addr_len"),
            ValueLayout.ADDRESS.withName("alpn_ptr"),
            ValueLayout.JAVA_LONG.withName("alpn_len")
    );

    private static final GroupLayout OPEN_STREAM_REQUEST = MemoryLayout.structLayout(
            ValueLayout.JAVA_LONG.withName("connection"),
            ValueLayout.JAVA_INT.withName("bidirectional"),
            ValueLayout.JAVA_INT.withName("reserved")
    );

    private static final GroupLayout SEND_REQUEST = MemoryLayout.structLayout(
            ValueLayout.JAVA_LONG.withName("stream"),
            ValueLayout.ADDRESS.withName("data_ptr"),
            ValueLayout.JAVA_LONG.withName("data_len"),
            ValueLayout.JAVA_LONG.withName("app_message_id"),
            ValueLayout.JAVA_INT.withName("flags"),
            ValueLayout.JAVA_INT.withName("reserved")
    );

    private static final GroupLayout EVENT_LAYOUT = MemoryLayout.structLayout(
            ValueLayout.JAVA_INT.withName("kind"),
            ValueLayout.JAVA_INT.withName("status"),
            ValueLayout.JAVA_LONG.withName("operation"),
            ValueLayout.JAVA_LONG.withName("object"),
            ValueLayout.JAVA_LONG.withName("related"),
            ValueLayout.JAVA_LONG.withName("app_message_id"),
            ValueLayout.ADDRESS.withName("data_ptr"),
            ValueLayout.JAVA_LONG.withName("data_len"),
            ValueLayout.JAVA_INT.withName("error_code"),
            ValueLayout.JAVA_INT.withName("flags")
    );

    private static final long EVENT_SIZE = EVENT_LAYOUT.byteSize();

    private final Arena sharedArena;
    private final SymbolLookup symbols;

    private final MethodHandle mhRuntimeNew;
    private final MethodHandle mhRuntimeClose;
    private final MethodHandle mhSecretKeyGenerate;
    private final MethodHandle mhEndpointCreate;
    private final MethodHandle mhEndpointClose;
    private final MethodHandle mhEndpointExportSecretKey;
    private final MethodHandle mhConnect;
    private final MethodHandle mhStreamOpen;
    private final MethodHandle mhStreamSend;
    private final MethodHandle mhPollEvents;
    private final MethodHandle mhReleaseEventData;

    private final long runtime;
    private final ExecutorService poller;
    private final ConcurrentMap<Long, CompletableFuture<BridgeEvent>> pending = new ConcurrentHashMap<>();
    private final ConcurrentMap<Long, StreamInbox> streamInboxes = new ConcurrentHashMap<>();
    private volatile boolean closed;

    public IrohNative(String libraryPath) {
        this.sharedArena = Arena.ofShared();
        System.load(libraryPath);
        this.symbols = SymbolLookup.loaderLookup();

        mhRuntimeNew = downcall("iroh_runtime_new",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        mhRuntimeClose = downcall("iroh_runtime_close",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG));
        mhSecretKeyGenerate = downcall("iroh_secret_key_generate",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        mhEndpointCreate = downcall("iroh_endpoint_create",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        mhEndpointClose = downcall("iroh_endpoint_close",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));
        mhEndpointExportSecretKey = downcall("iroh_endpoint_export_secret_key",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        mhConnect = downcall("iroh_connect",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        mhStreamOpen = downcall("iroh_stream_open",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        mhStreamSend = downcall("iroh_stream_send",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        mhPollEvents = downcall("iroh_poll_events",
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT));
        mhReleaseEventData = downcall("iroh_release_event_data",
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));

        this.runtime = createRuntime();
        this.poller = Executors.newSingleThreadExecutor(r -> {
            Thread t = new Thread(r, "iroh-bridge-poller");
            t.setDaemon(true);
            return t;
        });
        this.poller.submit(this::pollLoop);
    }

    public byte[] generateSecretKey() {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(64);
            MemorySegment outLen = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) mhSecretKeyGenerate.invokeExact(out, 64L, outLen);
            long needed = outLen.get(ValueLayout.JAVA_LONG, 0);
            if (status == IROH_STATUS_BUFFER_TOO_SMALL) {
                out = arena.allocate(needed);
                status = (int) mhSecretKeyGenerate.invokeExact(out, needed, outLen);
            }
            check(status, "iroh_secret_key_generate");
            int len = Math.toIntExact(outLen.get(ValueLayout.JAVA_LONG, 0));
            return out.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
        } catch (Throwable t) {
            throw new BridgeException("secret key generation failed", t);
        }
    }

    public CompletableFuture<Long> createEndpoint(EndpointOptions options) {
        Objects.requireNonNull(options, "options");
        try {
            Arena arena = Arena.ofConfined();
            MemorySegment cfg = arena.allocate(ENDPOINT_CONFIG);

            MemorySegment secretPtr = MemorySegment.NULL;
            long secretLen = 0;
            if (options.secretKey() != null && options.secretKey().length > 0) {
                MemorySegment secret = arena.allocateArray(ValueLayout.JAVA_BYTE, options.secretKey());
                secretPtr = secret;
                secretLen = options.secretKey().length;
            }

            List<String> relayUrls = options.relayUrls() == null ? List.of() : options.relayUrls();
            MemorySegment relayArray = MemorySegment.NULL;
            if (!relayUrls.isEmpty()) {
                MemorySegment relayArrayStorage = arena.allocateArray(ValueLayout.ADDRESS, relayUrls.size());
                for (int i = 0; i < relayUrls.size(); i++) {
                    MemorySegment s = arena.allocateUtf8String(relayUrls.get(i));
                    relayArrayStorage.setAtIndex(ValueLayout.ADDRESS, i, s);
                }
                relayArray = relayArrayStorage;
            }

            cfg.set(ValueLayout.ADDRESS, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("secret_key_ptr")), secretPtr);
            cfg.set(ValueLayout.JAVA_LONG, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("secret_key_len")), secretLen);
            cfg.set(ValueLayout.ADDRESS, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("relay_url_ptrs")), relayArray);
            cfg.set(ValueLayout.JAVA_LONG, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("relay_url_count")), relayUrls.size());
            cfg.set(ValueLayout.JAVA_INT, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("relay_mode")), options.relayMode());
            cfg.set(ValueLayout.JAVA_INT, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("enable_default_discovery")), options.enableDefaultDiscovery() ? 1 : 0);
            cfg.set(ValueLayout.JAVA_INT, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("reserved0")), 0);
            cfg.set(ValueLayout.JAVA_LONG, ENDPOINT_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("reserved1")), 0L);

            MemorySegment outOperation = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) mhEndpointCreate.invokeExact(runtime, cfg, outOperation);
            check(status, "iroh_endpoint_create");
            long op = outOperation.get(ValueLayout.JAVA_LONG, 0);

            CompletableFuture<BridgeEvent> base = registerPending(op, arena);
            return base.thenApply(event -> {
                if (event.kind() != IROH_EVENT_ENDPOINT_CREATED) {
                    throw new CompletionException(event.asException("endpoint creation failed"));
                }
                return event.object();
            });
        } catch (Throwable t) {
            return CompletableFuture.failedFuture(new BridgeException("endpoint create submit failed", t));
        }
    }

    public byte[] exportEndpointSecretKey(long endpointHandle) {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(64);
            MemorySegment outLen = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) mhEndpointExportSecretKey.invokeExact(runtime, endpointHandle, out, 64L, outLen);
            long needed = outLen.get(ValueLayout.JAVA_LONG, 0);
            if (status == IROH_STATUS_BUFFER_TOO_SMALL) {
                out = arena.allocate(needed);
                status = (int) mhEndpointExportSecretKey.invokeExact(runtime, endpointHandle, out, needed, outLen);
            }
            check(status, "iroh_endpoint_export_secret_key");
            int len = Math.toIntExact(outLen.get(ValueLayout.JAVA_LONG, 0));
            return out.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
        } catch (Throwable t) {
            throw new BridgeException("secret key export failed", t);
        }
    }

    public CompletableFuture<Long> connect(long endpointHandle, byte[] remoteAddrBytes, String alpn) {
        Objects.requireNonNull(remoteAddrBytes, "remoteAddrBytes");
        Objects.requireNonNull(alpn, "alpn");
        try {
            Arena arena = Arena.ofConfined();
            MemorySegment req = arena.allocate(CONNECT_REQUEST);
            MemorySegment remote = arena.allocateArray(ValueLayout.JAVA_BYTE, remoteAddrBytes);
            byte[] alpnBytes = alpn.getBytes(StandardCharsets.UTF_8);
            MemorySegment alpnSeg = arena.allocateArray(ValueLayout.JAVA_BYTE, alpnBytes);

            req.set(ValueLayout.ADDRESS, CONNECT_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("remote_addr_ptr")), remote);
            req.set(ValueLayout.JAVA_LONG, CONNECT_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("remote_addr_len")), remoteAddrBytes.length);
            req.set(ValueLayout.ADDRESS, CONNECT_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("alpn_ptr")), alpnSeg);
            req.set(ValueLayout.JAVA_LONG, CONNECT_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("alpn_len")), alpnBytes.length);

            MemorySegment outOperation = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) mhConnect.invokeExact(runtime, endpointHandle, req, outOperation);
            check(status, "iroh_connect");
            long op = outOperation.get(ValueLayout.JAVA_LONG, 0);

            CompletableFuture<BridgeEvent> base = registerPending(op, arena);
            return base.thenApply(event -> {
                if (event.kind() != IROH_EVENT_CONNECT_SUCCEEDED) {
                    throw new CompletionException(event.asException("connect failed"));
                }
                return event.object();
            });
        } catch (Throwable t) {
            return CompletableFuture.failedFuture(new BridgeException("connect submit failed", t));
        }
    }

    public CompletableFuture<Long> openBidirectionalStream(long connectionHandle) {
        try {
            Arena arena = Arena.ofConfined();
            MemorySegment req = arena.allocate(OPEN_STREAM_REQUEST);
            req.set(ValueLayout.JAVA_LONG, OPEN_STREAM_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("connection")), connectionHandle);
            req.set(ValueLayout.JAVA_INT, OPEN_STREAM_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("bidirectional")), 1);
            req.set(ValueLayout.JAVA_INT, OPEN_STREAM_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("reserved")), 0);

            MemorySegment outOperation = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) mhStreamOpen.invokeExact(runtime, req, outOperation);
            check(status, "iroh_stream_open");
            long op = outOperation.get(ValueLayout.JAVA_LONG, 0);

            CompletableFuture<BridgeEvent> base = registerPending(op, arena);
            return base.thenApply(event -> {
                if (event.kind() != IROH_EVENT_STREAM_OPEN_SUCCEEDED) {
                    throw new CompletionException(event.asException("open stream failed"));
                }
                streamInboxes.put(event.object(), new StreamInbox());
                return event.object();
            });
        } catch (Throwable t) {
            return CompletableFuture.failedFuture(new BridgeException("stream open submit failed", t));
        }
    }

    public CompletableFuture<Void> send(long streamHandle, ByteBuffer payload, long appMessageId, int flags) {
        Objects.requireNonNull(payload, "payload");
        try {
            byte[] bytes = new byte[payload.remaining()];
            payload.slice().get(bytes);

            Arena arena = Arena.ofConfined();
            MemorySegment req = arena.allocate(SEND_REQUEST);
            MemorySegment data = arena.allocateArray(ValueLayout.JAVA_BYTE, bytes);

            req.set(ValueLayout.JAVA_LONG, SEND_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("stream")), streamHandle);
            req.set(ValueLayout.ADDRESS, SEND_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("data_ptr")), data);
            req.set(ValueLayout.JAVA_LONG, SEND_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("data_len")), bytes.length);
            req.set(ValueLayout.JAVA_LONG, SEND_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("app_message_id")), appMessageId);
            req.set(ValueLayout.JAVA_INT, SEND_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("flags")), flags);
            req.set(ValueLayout.JAVA_INT, SEND_REQUEST.byteOffset(MemoryLayout.PathElement.groupElement("reserved")), 0);

            MemorySegment outOperation = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) mhStreamSend.invokeExact(runtime, req, outOperation);
            check(status, "iroh_stream_send");
            long op = outOperation.get(ValueLayout.JAVA_LONG, 0);

            CompletableFuture<BridgeEvent> base = registerPending(op, arena);
            return base.thenApply(event -> {
                if (event.kind() != IROH_EVENT_SEND_COMPLETED) {
                    throw new CompletionException(event.asException("send failed"));
                }
                return null;
            });
        } catch (Throwable t) {
            return CompletableFuture.failedFuture(new BridgeException("send submit failed", t));
        }
    }

    public SubmissionPublisher<ByteBuffer> incomingFrames(long streamHandle) {
        StreamInbox inbox = streamInboxes.computeIfAbsent(streamHandle, ignored -> new StreamInbox());
        return inbox.publisher();
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        poller.shutdownNow();
        try {
            mhRuntimeClose.invokeExact(runtime);
        } catch (Throwable ignored) {
        }
        sharedArena.close();
    }

    private MethodHandle downcall(String name, FunctionDescriptor fd) {
        MemorySegment symbol = symbols.find(name)
                .orElseThrow(() -> new IllegalStateException("Missing symbol: " + name));
        return LINKER.downcallHandle(symbol, fd);
    }

    private long createRuntime() {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment cfg = arena.allocate(RUNTIME_CONFIG);
            cfg.set(ValueLayout.JAVA_INT, RUNTIME_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("event_queue_capacity")), 4096);
            cfg.set(ValueLayout.JAVA_INT, RUNTIME_CONFIG.byteOffset(MemoryLayout.PathElement.groupElement("reserved")), 0);
            MemorySegment out = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) mhRuntimeNew.invokeExact(cfg, out);
            check(status, "iroh_runtime_new");
            return out.get(ValueLayout.JAVA_LONG, 0);
        } catch (Throwable t) {
            throw new BridgeException("runtime init failed", t);
        }
    }

    private CompletableFuture<BridgeEvent> registerPending(long operation, Arena ownerArena) {
        CompletableFuture<BridgeEvent> future = new CompletableFuture<>();
        pending.put(operation, future);
        future.whenComplete((ok, ex) -> ownerArena.close());
        return future;
    }

    private void pollLoop() {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment events = arena.allocate(EVENT_LAYOUT, 128);
            while (!closed) {
                long count = (long) mhPollEvents.invokeExact(runtime, events, 128L, 250);
                for (int i = 0; i < count; i++) {
                    MemorySegment ev = events.asSlice(i * EVENT_SIZE, EVENT_SIZE);
                    BridgeEvent event = decodeEvent(ev);
                    routeEvent(event);
                }
            }
        } catch (Throwable t) {
            pending.values().forEach(f -> f.completeExceptionally(new BridgeException("poll loop failed", t)));
            pending.clear();
        }
    }

    private void routeEvent(BridgeEvent event) {
        CompletableFuture<BridgeEvent> future = pending.remove(event.operation());
        if (future != null) {
            future.complete(event);
            return;
        }

        if (event.kind() == IROH_EVENT_FRAME_RECEIVED && event.object() != 0) {
            StreamInbox inbox = streamInboxes.computeIfAbsent(event.object(), ignored -> new StreamInbox());
            inbox.publisher().submit(event.payload());
        }
    }

    private BridgeEvent decodeEvent(MemorySegment ev) throws Throwable {
        int kind = ev.get(ValueLayout.JAVA_INT, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("kind")));
        int status = ev.get(ValueLayout.JAVA_INT, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("status")));
        long operation = ev.get(ValueLayout.JAVA_LONG, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("operation")));
        long object = ev.get(ValueLayout.JAVA_LONG, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("object")));
        long related = ev.get(ValueLayout.JAVA_LONG, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("related")));
        long appMessageId = ev.get(ValueLayout.JAVA_LONG, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("app_message_id")));
        MemorySegment dataPtr = ev.get(ValueLayout.ADDRESS, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("data_ptr")));
        long dataLen = ev.get(ValueLayout.JAVA_LONG, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("data_len")));
        int errorCode = ev.get(ValueLayout.JAVA_INT, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("error_code")));
        int flags = ev.get(ValueLayout.JAVA_INT, EVENT_LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("flags")));

        ByteBuffer payload = ByteBuffer.allocate(0);
        if (dataPtr.address() != 0 && dataLen > 0) {
            payload = dataPtr.reinterpret(dataLen).asByteBuffer().order(ByteOrder.nativeOrder());
            ByteBuffer owned = ByteBuffer.allocate((int) dataLen);
            owned.put(payload.duplicate());
            owned.flip();
            mhReleaseEventData.invokeExact(runtime, dataPtr, dataLen);
            payload = owned.asReadOnlyBuffer();
        }

        return new BridgeEvent(kind, status, operation, object, related, appMessageId, payload, errorCode, flags);
    }

    private static void check(int status, String op) {
        if (status != IROH_STATUS_OK) {
            throw new BridgeException(op + " returned status " + status);
        }
    }

    public record EndpointOptions(byte[] secretKey, List<String> relayUrls, int relayMode,
                                  boolean enableDefaultDiscovery) {
        public EndpointOptions {
            relayUrls = relayUrls == null ? List.of() : List.copyOf(relayUrls);
        }

        public static EndpointOptions defaults() {
            return new EndpointOptions(null, List.of(), IROH_RELAY_MODE_DEFAULT, true);
        }

        public EndpointOptions withSecretKey(byte[] secret) {
            return new EndpointOptions(secret, relayUrls, relayMode, enableDefaultDiscovery);
        }

        public EndpointOptions withCustomRelays(List<String> relays) {
            return new EndpointOptions(secretKey, relays, IROH_RELAY_MODE_CUSTOM, enableDefaultDiscovery);
        }
    }

    public record BridgeEvent(int kind, int status, long operation, long object, long related,
                              long appMessageId, ByteBuffer payload, int errorCode, int flags) {
        public BridgeException asException(String prefix) {
            String message = prefix + " [kind=" + kind + ", status=" + status + ", errorCode=" + errorCode + "]";
            if (payload != null && payload.remaining() > 0) {
                byte[] bytes = new byte[payload.remaining()];
                payload.duplicate().get(bytes);
                message += " " + new String(bytes, StandardCharsets.UTF_8);
            }
            return new BridgeException(message);
        }
    }

    public static final class StreamInbox {
        private final SubmissionPublisher<ByteBuffer> publisher = new SubmissionPublisher<>();
        public SubmissionPublisher<ByteBuffer> publisher() { return publisher; }
    }

    public static class BridgeException extends RuntimeException {
        public BridgeException(String message) { super(message); }
        public BridgeException(String message, Throwable cause) { super(message, cause); }
    }
}
