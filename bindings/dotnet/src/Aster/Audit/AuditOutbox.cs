using System.Runtime.InteropServices;
using System.Buffers;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Aster.Audit;

/// <summary>Consumer-operated endpoint implementing Aster's durable-ACK protocol.</summary>
public sealed record AuditDestination(string Endpoint, string? ReceiverIdentity = null, string? Authorization = null)
{
    public override string ToString() => "AuditDestination { redacted }";
}
public sealed record AuditOutboxConfig(string Directory, AuditDestination Destination,
    ulong MaxRetainedBytes = 256UL * 1024 * 1024, ulong MaxEventBytes = 64UL * 1024);
public sealed record AuditEvent(ulong TimestampMs, string Kind, JsonElement Payload);
public enum AuditErrorKind
{
    InvalidConfig = 1, InvalidEvent, EventTooLarge, StorageFull, Storage, CommitUncertain,
    QueueOverloaded, Maintenance, Closed, WorkerFailed, Timeout, DeliveryStalled,
    Disposition, NotFound, DestinationMismatch, IntegrityConflict, StaleDelivery
}
public sealed class AuditOutboxException(AuditErrorKind kind, string message) : Exception(message)
{
    public AuditErrorKind Kind { get; } = kind;
}
public sealed record AuditDeliveryError(string Kind, ushort? HttpStatus, ulong Sequence,
    bool ImmediateFlushFailure, string Message);
public sealed record AuditOutboxStatus(
    string OutboxId, ulong CommittedSequence, ulong PendingRecords, ulong PendingBytes,
    ulong DeadLetterRecords, ulong DeadLetterBytes, ulong DispositionReceipts, ulong DispositionBytes,
    ulong RetainedBytes, ulong MaxRetainedBytes, ulong QueuedRecords, ulong QueuedBytes,
    ulong? OldestAgeMs, bool Accepting, bool Ready, string DeliveryState,
    AuditDeliveryError? LastDeliveryError, bool StorageWorkerAlive, bool DeliveryWorkerAlive,
    bool Maintenance, ulong? MaintenanceDurationMs, bool Closed, ulong DurableCommits, ulong LastGroupSize,
    bool StorageRecovered);
public sealed record AuditDispositionReceipt(string EventId, string Action, ulong TimestampMs,
    string Reason, string OperatorReference, string DestinationFingerprint, AuditDeliveryError? Rejection);
/// <summary>
/// One event the receiver acknowledged as durably stored (ABI 7.1). The outbox
/// writes it in the same local transaction that deletes the event, so an event is
/// either still pending or present here until the log's count/age bounds prune it.
/// <see cref="Cursor"/> increases with each acknowledgement and is never reused.
/// </summary>
public sealed record AuditAcknowledgement(ulong Cursor, string EventId, ulong Sequence, string BatchId,
    ulong AcknowledgedAtMs)
{
    /// <summary><see cref="AcknowledgedAtMs"/> (local Unix milliseconds of the commit) as a timestamp.</summary>
    public DateTimeOffset AcknowledgedAt => DateTimeOffset.FromUnixTimeMilliseconds(checked((long)AcknowledgedAtMs));
}
public sealed record AuditQuarantineExport(string EventId, string EnvelopeBase64, bool Pending,
    IReadOnlyList<AuditDispositionReceipt> Receipts);

/// <summary>
/// Thin async wrapper over the shared Rust durable outbox. Submit success is local
/// durability; Flush confirms receiver acceptance. Cancellation abandons the wait,
/// never retracts accepted events. Clones share workers and budgets.
/// </summary>
public sealed class AuditOutbox : IDisposable, IAsyncDisposable
{
    private const uint Opened = 114, Submitted = 115, StatusResult = 116, Exported = 117, Acknowledged = 128;
    /// <summary>The largest page one acknowledgement read returns.</summary>
    public const int AcknowledgementPageMax = 1000;
    /// <summary>The longest wait one acknowledgement read accepts (<c>ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS</c>).</summary>
    public static readonly TimeSpan AcknowledgementWaitMax = TimeSpan.FromMilliseconds(Native.AuditOutboxMaxAckWaitMs);
    private static readonly JsonSerializerOptions Json = new() { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower, MaxDepth = 128 };
    private readonly Runtime _runtime;
    private readonly OutboxHandle _handle;
    private readonly object _closeLock = new();
    private Task? _closeTask;
    private readonly int _eventJsonLimit;
    private const int ConfigJsonLimit = 32 * 1024, OperatorJsonLimit = 8 * 1024;
    private const int MaxEventBytes = 1024 * 1024;

    private sealed class OutboxHandle(Runtime runtime, ulong value) : SafeHandle(unchecked((IntPtr)(long)value), true)
    {
        public override bool IsInvalid => handle == IntPtr.Zero;
        protected override bool ReleaseHandle()
        {
            Native.aster_audit_outbox_free(runtime.Handle, unchecked((ulong)handle.ToInt64()));
            return true;
        }
    }
    private AuditOutbox(Runtime runtime, ulong handle, int eventJsonLimit)
    { _runtime = runtime; _handle = new(runtime, handle); _eventJsonLimit = eventJsonLimit; }
    private ulong Handle => !_handle.IsClosed ? unchecked((ulong)_handle.DangerousGetHandle().ToInt64())
        : throw new ObjectDisposedException(nameof(AuditOutbox));

    private delegate int JsonCall(Bytes data, out ulong operation);
    // Utf8JsonWriter writes directly to bounded storage; no GetRawText or full
    // serialized JSON copy is made. Reservation slack covers its escaping and
    // chunk estimates; Advance enforces the actual encoded input ceiling.
    private sealed class BoundedAuditBuffer(int limit, AuditErrorKind kind) : IBufferWriter<byte>
    {
        private byte[] _bytes = new byte[Math.Min(limit, 4096)];
        private int _written;
        public ReadOnlyMemory<byte> Written => _bytes.AsMemory(0, _written);
        public void Advance(int count)
        {
            if (count < 0 || count > limit - _written) throw Oversize();
            _written += count;
        }
        private AuditOutboxException Oversize() => new(kind, "encoded audit request exceeds its input limit");
        private void Ensure(int hint)
        {
            hint = Math.Max(hint, 1);
            int cap = checked(limit * 6 + 4096);
            if (hint > cap - _written) throw Oversize();
            int needed = _written + hint;
            if (needed > _bytes.Length) Array.Resize(ref _bytes, Math.Min(cap, Math.Max(needed, _bytes.Length * 2)));
        }
        public Memory<byte> GetMemory(int sizeHint = 0) { Ensure(sizeHint); return _bytes.AsMemory(_written); }
        public Span<byte> GetSpan(int sizeHint = 0) { Ensure(sizeHint); return _bytes.AsSpan(_written); }
    }
    [JsonConverter(typeof(PayloadLengthConverter))]
    private readonly record struct PayloadLength(long Length);
    // JsonElement.Deserialize reads its existing UTF-8 memory directly. Skip
    // counts bytes without decoding strings or copying their raw text; this
    // protects against the encoder allocating for a huge escaped token before
    // it asks the bounded output writer for space.
    private sealed class PayloadLengthConverter : JsonConverter<PayloadLength>
    {
        public override PayloadLength Read(ref Utf8JsonReader reader, Type type, JsonSerializerOptions options)
        {
            long start = reader.TokenStartIndex;
            reader.Skip();
            return new(reader.BytesConsumed - start);
        }
        public override void Write(Utf8JsonWriter writer, PayloadLength value, JsonSerializerOptions options)
            => throw new NotSupportedException();
    }
    internal static ReadOnlyMemory<byte> SerializeRequest(string key, object value, int limit, AuditErrorKind kind)
    {
        if (value is AuditEvent auditEvent)
        {
            if (auditEvent.Payload.ValueKind != JsonValueKind.Object)
                throw new AuditOutboxException(AuditErrorKind.InvalidEvent, "audit payload must be an object");
            long length;
            try { length = auditEvent.Payload.Deserialize<PayloadLength>(Json).Length; }
            catch (JsonException) { throw new AuditOutboxException(AuditErrorKind.InvalidEvent, "invalid audit payload nesting"); }
            if (length > limit - 1024) throw new AuditOutboxException(AuditErrorKind.EventTooLarge, "audit payload exceeds event limit");
        }
        var buffer = new BoundedAuditBuffer(limit, kind);
        using var writer = new Utf8JsonWriter(buffer);
        writer.WriteStartObject(); writer.WriteNumber("version", 1); writer.WritePropertyName(key);
        JsonSerializer.Serialize(writer, value, value.GetType(), Json);
        writer.WriteEndObject(); writer.Flush();
        return buffer.Written;
    }
    private static void Text(string? value, int min, int max, AuditErrorKind kind)
    {
        if (value is null || value.Length < min || value.Length > max || Encoding.UTF8.GetByteCount(value) > max)
            throw new AuditOutboxException(kind, "audit argument exceeds its text bounds");
    }
    private static void Destination(AuditDestination destination)
    {
        if (destination is null) throw new AuditOutboxException(AuditErrorKind.InvalidConfig, "audit destination is required");
        Text(destination.Endpoint, 1, 4096, AuditErrorKind.InvalidConfig);
        if (destination.ReceiverIdentity is not null) Text(destination.ReceiverIdentity, 1, 128, AuditErrorKind.InvalidConfig);
        if (destination.Authorization is not null) Text(destination.Authorization, 0, 4096, AuditErrorKind.InvalidConfig);
    }
    private static ulong StartJson(string key, object value, int limit, AuditErrorKind kind, JsonCall call)
    {
        using var scope = new NativeScope();
        var data = SerializeRequest(key, value, limit, kind);
        Native.Check(call(scope.Bytes(data.Span), out ulong operation), "audit operation");
        return operation;
    }

    // Keep the source wait registered after caller cancellation so a completion
    // already polled out of Rust cannot leak its handle or output buffer.
    private static async Task<(Event Event, byte[] Data)> Await(Runtime rt, ulong op, uint expected, CancellationToken ct)
    {
        Task<Event> source = rt.WaitForAsync(op);
        Event ev;
        try { ev = await source.WaitAsync(ct).ConfigureAwait(false); }
        catch (OperationCanceledException)
        {
            _ = CleanupLate(rt, source);
            throw;
        }
        byte[] data = EventError.TakePayload(rt, in ev);
        if (ev.kind == (uint)EventKind.Error)
            throw new AuditOutboxException((AuditErrorKind)ev.error_code, Encoding.UTF8.GetString(data));
        if (ev.kind != expected)
        {
            if (ev.kind == Opened && ev.handle != 0) Native.aster_audit_outbox_free(rt.Handle, ev.handle);
            throw new IrohException($"Unexpected audit completion {ev.kind}");
        }
        return (ev, data);
    }
    private static async Task CleanupLate(Runtime rt, Task<Event> source)
    {
        try
        {
            var ev = await source.ConfigureAwait(false);
            if (ev.kind == Opened && ev.handle != 0) Native.aster_audit_outbox_free(rt.Handle, ev.handle);
            EventError.TakePayload(rt, in ev);
        }
        catch (Exception) { /* Runtime teardown owns remaining native resources. */ }
    }

    public static async Task<AuditOutbox> OpenAsync(Runtime runtime, AuditOutboxConfig config, CancellationToken ct = default)
    {
        ArgumentNullException.ThrowIfNull(runtime);
        ArgumentNullException.ThrowIfNull(config);
        ct.ThrowIfCancellationRequested();
        Text(config.Directory, 1, ConfigJsonLimit, AuditErrorKind.InvalidConfig);
        Destination(config.Destination);
        if (config.MaxEventBytes < 256 || config.MaxEventBytes > MaxEventBytes || config.MaxRetainedBytes < 256)
            throw new AuditOutboxException(AuditErrorKind.InvalidConfig, "invalid audit storage/event limits");
        int eventLimit = checked((int)config.MaxEventBytes + 1024);
        ulong op = StartJson("config", config, ConfigJsonLimit, AuditErrorKind.InvalidConfig, (Bytes data, out ulong id) => Native.aster_audit_outbox_open(runtime.Handle, data, 0, out id));
        var (ev, _) = await Await(runtime, op, Opened, ct).ConfigureAwait(false);
        return new(runtime, ev.handle, eventLimit);
    }
    public async Task<string> SubmitAsync(AuditEvent auditEvent, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Text(auditEvent.Kind, 1, 128, AuditErrorKind.InvalidEvent);
        ulong handle = Handle;
        ulong op = StartJson("event", auditEvent, _eventJsonLimit, AuditErrorKind.EventTooLarge, (Bytes data, out ulong id) => Native.aster_audit_outbox_submit(_runtime.Handle, handle, data, 0, out id));
        var (_, data) = await Await(_runtime, op, Submitted, ct).ConfigureAwait(false);
        return Encoding.UTF8.GetString(data);
    }
    public async Task<AuditOutboxStatus> StatusAsync(CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Native.Check(Native.aster_audit_outbox_status(_runtime.Handle, Handle, 0, out ulong op), "audit status");
        var (_, data) = await Await(_runtime, op, StatusResult, ct).ConfigureAwait(false);
        return Decode<AuditOutboxStatus>(data, "status");
    }
    private static T Decode<T>(byte[] data, string field)
    {
        using var document = JsonDocument.Parse(data);
        if (document.RootElement.GetProperty("version").GetInt32() != 1) throw new IrohException("Unsupported audit result version");
        return document.RootElement.GetProperty(field).Deserialize<T>(Json) ?? throw new IrohException("Missing audit result");
    }
    public async Task FlushAsync(TimeSpan timeout, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        if (timeout < TimeSpan.Zero) throw new ArgumentOutOfRangeException(nameof(timeout));
        ulong ms = (ulong)(timeout.Ticks / TimeSpan.TicksPerMillisecond);
        Native.Check(Native.aster_audit_outbox_flush(_runtime.Handle, Handle, ms, 0, out ulong op), "audit flush");
        await Await(_runtime, op, (uint)EventKind.UnitResult, ct).ConfigureAwait(false);
    }
    public Task ReconfigureAsync(AuditDestination destination, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Destination(destination);
        ulong handle = Handle;
        return Unit(StartJson("destination", destination, ConfigJsonLimit, AuditErrorKind.InvalidConfig, (Bytes data, out ulong id) => Native.aster_audit_outbox_reconfigure(_runtime.Handle, handle, data, 0, out id)), ct);
    }
    public Task QuarantineAsync(string eventId, string reason, string operatorReference, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Text(eventId, 1, 133, AuditErrorKind.IntegrityConflict);
        Text(reason, 1, 512, AuditErrorKind.InvalidEvent);
        Text(operatorReference, 1, 256, AuditErrorKind.InvalidEvent);
        ulong handle = Handle;
        return Unit(StartJson("quarantine", new { eventId, reason, operatorReference }, OperatorJsonLimit, AuditErrorKind.InvalidEvent, (Bytes data, out ulong id) => Native.aster_audit_outbox_quarantine(_runtime.Handle, handle, data, 0, out id)), ct);
    }
    public Task ReplayAsync(string eventId, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Text(eventId, 1, 133, AuditErrorKind.IntegrityConflict);
        ulong handle = Handle;
        return Unit(StartJson("event_id", eventId, OperatorJsonLimit, AuditErrorKind.IntegrityConflict, (Bytes data, out ulong id) => Native.aster_audit_outbox_replay(_runtime.Handle, handle, data, 0, out id)), ct);
    }
    public async Task<AuditQuarantineExport> ExportAsync(string eventId, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Text(eventId, 1, 133, AuditErrorKind.IntegrityConflict);
        ulong handle = Handle;
        ulong op = StartJson("event_id", eventId, OperatorJsonLimit, AuditErrorKind.IntegrityConflict, (Bytes data, out ulong id) => Native.aster_audit_outbox_export(_runtime.Handle, handle, data, 0, out id));
        var (_, data) = await Await(_runtime, op, Exported, ct).ConfigureAwait(false);
        return Decode<AuditQuarantineExport>(data, "export");
    }
    /// <summary>
    /// Up to <paramref name="limit"/> (1..<see cref="AcknowledgementPageMax"/>) logged acknowledgements
    /// with a cursor greater than <paramref name="after"/>, oldest first. Pass 0 to start at the oldest
    /// retained entry.
    /// </summary>
    public Task<IReadOnlyList<AuditAcknowledgement>> AcknowledgementsAsync(ulong after = 0, int limit = AcknowledgementPageMax,
        CancellationToken ct = default) => WaitAcknowledgementsAsync(after, limit, TimeSpan.Zero, ct);

    /// <summary>
    /// <see cref="AcknowledgementsAsync"/>, but when nothing follows <paramref name="after"/> it waits up to
    /// <paramref name="wait"/> (at most <see cref="AcknowledgementWaitMax"/>) for the next acknowledgement,
    /// returning an empty page when the wait ends first. Cancellation abandons the wait.
    /// </summary>
    public async Task<IReadOnlyList<AuditAcknowledgement>> WaitAcknowledgementsAsync(ulong after, int limit, TimeSpan wait,
        CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        if (limit < 1 || limit > AcknowledgementPageMax)
            throw new AuditOutboxException(AuditErrorKind.InvalidEvent, "acknowledgement limit must be between 1 and 1000");
        if (wait < TimeSpan.Zero || wait > AcknowledgementWaitMax)
            throw new AuditOutboxException(AuditErrorKind.InvalidEvent, "acknowledgement wait must be between 0 and one hour");
        ulong handle = Handle;
        byte[] request = JsonSerializer.SerializeToUtf8Bytes(new
        {
            version = 1, after, limit, wait_ms = (ulong)(wait.Ticks / TimeSpan.TicksPerMillisecond),
        });
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_audit_outbox_acknowledgements(_runtime.Handle, handle, scope.Bytes(request), 0, out op), "audit acknowledgements");
        var (_, data) = await Await(_runtime, op, Acknowledged, ct).ConfigureAwait(false);
        return ParseAcknowledgements(data);
    }

    internal static IReadOnlyList<AuditAcknowledgement> ParseAcknowledgements(ReadOnlySpan<byte> data)
    {
        var page = JsonSerializer.Deserialize<AcknowledgementPage>(data, Json) ?? throw new IrohException("Missing audit acknowledgements");
        if (page.Version != 1) throw new IrohException("Unsupported audit acknowledgements version");
        return page.Acknowledgements ?? [];
    }
    private sealed record AcknowledgementPage(int Version, List<AuditAcknowledgement>? Acknowledgements);

    /// <summary>
    /// Streams acknowledgements with a cursor greater than <paramref name="after"/> until
    /// <paramref name="ct"/> is cancelled (the enumeration then ends without throwing) or the outbox
    /// fails (it throws). Delivery is lossless while entries are retained: record the last
    /// <see cref="AuditAcknowledgement.Cursor"/> to resume after a restart.
    /// </summary>
    public async IAsyncEnumerable<AuditAcknowledgement> WatchAcknowledgementsAsync(ulong after = 0,
        [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken ct = default)
    {
        while (!ct.IsCancellationRequested)
        {
            IReadOnlyList<AuditAcknowledgement> page;
            try { page = await WaitAcknowledgementsAsync(after, AcknowledgementPageMax, TimeSpan.FromSeconds(30), ct).ConfigureAwait(false); }
            catch (OperationCanceledException) when (ct.IsCancellationRequested) { yield break; }
            foreach (var ack in page)
            {
                if (ct.IsCancellationRequested) yield break;
                after = ack.Cursor;
                yield return ack;
            }
        }
    }
    private async Task Unit(ulong op, CancellationToken ct) => await Await(_runtime, op, (uint)EventKind.UnitResult, ct).ConfigureAwait(false);
    public AuditOutbox Clone()
    {
        Native.Check(Native.aster_audit_outbox_clone(_runtime.Handle, Handle, out ulong clone), "audit clone");
        return new(_runtime, clone, _eventJsonLimit);
    }
    /// <summary>Stops the shared workers and awaits storage release. Cancellation abandons only the wait.</summary>
    public Task CloseAsync(CancellationToken ct = default)
    {
        lock (_closeLock)
        {
            // Start shutdown independently of this caller's cancellation. Every
            // concurrent closer observes the same underlying completion.
            _closeTask ??= _handle.IsClosed ? Task.CompletedTask : CloseCoreAsync();
            return _closeTask.WaitAsync(ct);
        }
    }
    private async Task CloseCoreAsync()
    {
        try
        {
            Native.Check(Native.aster_audit_outbox_close(_runtime.Handle, Handle, 0, out ulong op), "audit close");
            await Unit(op, CancellationToken.None).ConfigureAwait(false);
        }
        finally { Dispose(); }
    }
    /// <summary>Drops this handle; the last reference initiates asynchronous shutdown. Use CloseAsync to await it.</summary>
    public void Dispose() => _handle.Dispose();
    public async ValueTask DisposeAsync() => await CloseAsync().ConfigureAwait(false);
}
