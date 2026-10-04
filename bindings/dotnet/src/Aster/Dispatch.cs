using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary><c>aster_dispatch_event_kind_t</c>.</summary>
internal enum DispatchEventKind : byte
{
    Call = 0,
    Cancelled = 1,
    ResponseWritable = 2,
    ResponseClosed = 3,
    SessionClosed = 4,
    ConnectionClosed = 5,
    NonRpc = 6,
}

/// <summary><c>aster_cancel_reason_t</c> (the <c>reason</c> of a <c>CANCELLED</c> event).</summary>
internal enum DispatchCancelReason : byte
{
    /// <summary>The peer cancelled or the stream failed; no trailer was sent.</summary>
    Peer = 0,
    /// <summary>The deadline passed; Rust sent <c>DEADLINE_EXCEEDED</c>.</summary>
    Deadline = 1,
    /// <summary>A later request frame was corrupt; Rust sent <c>INVALID_ARGUMENT</c>.</summary>
    InvalidRequest = 2,
}

/// <summary><c>ASTER_DISPATCH_FLAG_*</c>.</summary>
[Flags]
internal enum DispatchFlags : uint
{
    None = 0,
    FirstIsEnd = 0x1,
    NewSession = 0x2,
    IrohAuthenticated = 0x4,
    HasMetadata = 0x100,
    HasAttributes = 0x200,
}

/// <summary><c>aster_server_option_t</c>.</summary>
internal enum DispatchServerOption : uint
{
    BuiltinNodeInfo = 1,
    BuiltinTopology = 2,
    MaxSessionsPerConnection = 3,
    HandlerTimeoutMs = 4,
}

/// <summary>
/// The dynamic-dispatch C ABI (3.5, <c>aster_server_*</c> / <c>aster_dispatch_*</c>; design
/// <c>docs/_internal/bindings/dynamic-dispatch-design.md</c> §4.4). Rust serves the registered
/// services through <c>aster::rpc::Server</c>: it decodes headers, routes, runs Gate 3, keeps
/// sessions per connection, enforces deadlines and frame limits, and serves the baseline
/// services. This class answers calls by <c>call_id</c> with payloads and AEv1 <c>RpcStatus</c>
/// bytes; Rust frames and compresses.
///
/// Threading: exactly one thread calls <see cref="Poll"/>; any thread may answer a call, one
/// producer per call. Response operations never block in native code: a full response queue
/// returns <c>QUEUE_FULL</c>, after which the exact byte count is reserved
/// (<c>aster_dispatch_reserve</c>) and the producer awaits a task that the poll thread completes
/// on <c>RESPONSE_WRITABLE</c> / <c>RESPONSE_CLOSED</c> (<see cref="HandleCapacityEvent"/>). The
/// retry consumes the held capacity, so it cannot be full again.
/// </summary>
internal sealed class DispatchServer : IDisposable
{
    /// <summary>sizeof(aster_dispatch_event_t) on LP64 (ffi/src/layout.rs).</summary>
    internal const int EventSize = 104;

    private readonly ulong _runtime;
    private readonly ulong _handle;
    private readonly ConcurrentDictionary<ulong, TaskCompletionSource<bool>> _capacityWaiters = new();
    private int _pollThreadId = -1;
    private volatile bool _disposed;

    private static long s_queueFullHits;

    /// <summary>Diagnostic: total <c>QUEUE_FULL</c> results observed in this process.</summary>
    public static long QueueFullHits => Interlocked.Read(ref s_queueFullHits);

    private DispatchServer(ulong runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary><c>aster_server_builder_new</c> … <c>aster_server_start</c>.</summary>
    internal sealed class Builder : IDisposable
    {
        private readonly ulong _runtime;
        private ulong _handle;

        public Builder(ulong runtime, ulong node)
        {
            _runtime = runtime;
            Native.Check(Native.aster_server_builder_new(runtime, node, out _handle), "aster_server_builder_new");
        }

        /// <summary>Registers one service (its JSON registration form); returns its <c>service_idx</c>.</summary>
        public unsafe uint AddServiceJson(string json)
        {
            byte[] bytes = Encoding.UTF8.GetBytes(json);
            uint idx;
            fixed (byte* p = bytes)
                Native.Check(Native.aster_server_add_service_json(_handle, p, (uint)bytes.Length, out idx), "aster_server_add_service_json");
            return idx;
        }

        public Builder Option(DispatchServerOption key, ulong value)
        {
            Native.Check(Native.aster_server_set_option(_handle, (uint)key, value), "aster_server_set_option");
            return this;
        }

        /// <summary>Starts serving; consumes the builder.</summary>
        public DispatchServer Start(uint ringCapacity)
        {
            ulong h = _handle;
            _handle = 0;
            Native.Check(Native.aster_server_start(_runtime, h, ringCapacity, out ulong server), "aster_server_start");
            return new DispatchServer(_runtime, server);
        }

        public void Dispose()
        {
            if (_handle != 0)
            {
                Native.aster_server_builder_free(_handle);
                _handle = 0;
            }
        }
    }

    /// <summary>The <c>contract_id</c> Rust derives from a service's JSON registration form.</summary>
    public static unsafe string ContractId(string json)
    {
        byte[] bytes = Encoding.UTF8.GetBytes(json);
        Bytes output = default;
        fixed (byte* p = bytes)
            Native.Check(Native.aster_dynamic_service_contract_id(p, (uint)bytes.Length, &output), "aster_dynamic_service_contract_id");
        return Encoding.UTF8.GetString(NativeScope.TakeOwned(ref output));
    }

    /// <summary>
    /// Drains up to <paramref name="max"/> events into <paramref name="events"/>, waiting up to
    /// <paramref name="timeoutMs"/> (0 = no wait). Single consumer.
    /// </summary>
    public unsafe uint Poll(DispatchEventNative* events, uint max, uint timeoutMs)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        _pollThreadId = Environment.CurrentManagedThreadId;
        return Native.aster_server_poll(_runtime, _handle, events, max, timeoutMs);
    }

    /// <summary>Releases an event / recv / metadata lease (0 is a no-op).</summary>
    public void BufferRelease(ulong buffer)
    {
        if (buffer != 0) Native.aster_dispatch_buffer_release(_runtime, _handle, buffer);
    }

    /// <summary>Live calls in the server's call table (diagnostics / tests).</summary>
    public ulong LiveCalls => Native.aster_server_live_calls(_handle);

    // ─── Responses (true = queued, false = the call is over) ────────────────

    private enum Op { Respond, Send, Finish }

    /// <summary>Terminal single-shot response: <paramref name="payload"/> (may be empty) plus the trailer (null = OK).</summary>
    public ValueTask<bool> RespondAsync(ulong callId, byte[]? payload, byte[]? status)
        => Submit(Op.Respond, callId, payload, status);

    /// <summary>One response data frame (server-stream / bidi); <paramref name="payload"/> must not be empty.</summary>
    public ValueTask<bool> SendAsync(ulong callId, byte[] payload)
        => Submit(Op.Send, callId, payload, null);

    /// <summary>The trailer (null = OK).</summary>
    public ValueTask<bool> FinishAsync(ulong callId, byte[]? status)
        => Submit(Op.Finish, callId, null, status);

    private ValueTask<bool> Submit(Op op, ulong callId, byte[]? payload, byte[]? status)
    {
        if (_disposed) return new ValueTask<bool>(false);
        int r = Attempt(op, callId, payload, status, learnLength: false, out _);
        if (r == (int)Status.QueueFull)
            return SubmitSlowAsync(op, callId, payload, status);
        return new ValueTask<bool>(Outcome(r, op));
    }

    private async ValueTask<bool> SubmitSlowAsync(Op op, ulong callId, byte[]? payload, byte[]? status)
    {
        Interlocked.Increment(ref s_queueFullHits);
        while (true)
        {
            // Learn the exact length (the retry may also just succeed).
            int r = Attempt(op, callId, payload, status, learnLength: true, out ulong required);
            if (r != (int)Status.QueueFull)
                return Outcome(r, op);
            Interlocked.Increment(ref s_queueFullHits);
            await AwaitCapacityAsync(callId, required).ConfigureAwait(false);
            if (_disposed) return false;
        }
    }

    private unsafe int Attempt(Op op, ulong callId, byte[]? payload, byte[]? status, bool learnLength, out ulong required)
    {
        ulong req = 0;
        ulong* reqPtr = learnLength ? &req : null;
        int plen = payload?.Length ?? 0;
        int slen = status?.Length ?? 0;
        int r;
        fixed (byte* p = payload)
        fixed (byte* s = status)
        {
            r = op switch
            {
                Op.Respond => Native.aster_dispatch_respond(_runtime, _handle, callId,
                    plen == 0 ? null : p, (uint)plen, slen == 0 ? null : s, (uint)slen, reqPtr),
                Op.Send => Native.aster_dispatch_send(_runtime, _handle, callId, p, (uint)plen, reqPtr),
                _ => Native.aster_dispatch_finish(_runtime, _handle, callId, slen == 0 ? null : s, (uint)slen, reqPtr),
            };
        }
        required = req;
        return r;
    }

    private static bool Outcome(int r, Op op)
    {
        if (r == 0) return true;
        if (r == (int)Status.NotFound || r == (int)Status.AlreadyClosed) return false;
        throw IrohException.FromStatus(r, op switch
        {
            Op.Respond => "aster_dispatch_respond",
            Op.Send => "aster_dispatch_send",
            _ => "aster_dispatch_finish",
        });
    }

    private Task AwaitCapacityAsync(ulong callId, ulong requiredLen)
    {
        if (Environment.CurrentManagedThreadId == _pollThreadId)
            throw new InvalidOperationException(
                "a dispatch response hit QUEUE_FULL on the server poll thread; waiting here would deadlock " +
                "because only the poll loop delivers RESPONSE_WRITABLE. Answer calls from another thread.");
        var waiter = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!_capacityWaiters.TryAdd(callId, waiter))
            throw new InvalidOperationException(
                $"concurrent response-capacity waits violate the one-producer contract for call {callId}");
        int s = Native.aster_dispatch_reserve(_runtime, _handle, callId, requiredLen);
        if (s != 0)
        {
            _capacityWaiters.TryRemove(new KeyValuePair<ulong, TaskCompletionSource<bool>>(callId, waiter));
            if (s == (int)Status.NotFound || s == (int)Status.AlreadyClosed)
                return Task.CompletedTask; // the retry reports the call as over
            throw IrohException.FromStatus(s, "aster_dispatch_reserve");
        }
        // RESPONSE_CLOSED completes with false: the retry then reports the call as over.
        return waiter.Task;
    }

    /// <summary>Completes a capacity waiter (poll thread: <c>RESPONSE_WRITABLE</c> / <c>RESPONSE_CLOSED</c>).</summary>
    public void HandleCapacityEvent(DispatchEventKind kind, ulong callId)
    {
        if (_capacityWaiters.TryRemove(callId, out var waiter))
            waiter.TrySetResult(kind == DispatchEventKind.ResponseWritable);
    }

    // ─── Requests ───────────────────────────────────────────────────────────

    /// <summary>Result of <see cref="Recv"/>.</summary>
    public enum RecvStatus { Payload, EndOfStream, Timeout, Gone }

    /// <summary>
    /// The next request payload after the first (client-stream / bidi), already decompressed.
    /// <paramref name="timeoutMs"/> 0 = do not wait. Blocks up to the timeout.
    /// </summary>
    public unsafe RecvStatus Recv(ulong callId, uint timeoutMs, out byte[] payload)
    {
        payload = Array.Empty<byte>();
        if (_disposed) return RecvStatus.Gone;
        byte* ptr = null;
        uint len = 0;
        ulong buffer = 0;
        int r = Native.aster_dispatch_recv(_runtime, _handle, callId, timeoutMs, &ptr, &len, &buffer);
        switch (r)
        {
            case 0:
                if (buffer == 0) return RecvStatus.EndOfStream;
                try { payload = len == 0 ? Array.Empty<byte>() : new ReadOnlySpan<byte>(ptr, (int)len).ToArray(); }
                finally { BufferRelease(buffer); }
                return RecvStatus.Payload;
            case (int)Status.Timeout: return RecvStatus.Timeout;
            case (int)Status.NotFound: return RecvStatus.Gone;
            default: throw IrohException.FromStatus(r, "aster_dispatch_recv");
        }
    }

    /// <summary>Whether the call was cancelled, finished or is unknown (stop producing).</summary>
    public bool IsCancelled(ulong callId)
        => _disposed || Native.aster_dispatch_is_cancelled(_runtime, _handle, callId) != 0;

    /// <summary>Ends the call with <c>INTERNAL "handler dropped call"</c> unless it already ended.</summary>
    public void Abandon(ulong callId)
    {
        if (!_disposed) Native.aster_dispatch_abandon(_runtime, _handle, callId);
    }

    /// <summary>The call header's metadata (empty when the call is over or has none).</summary>
    public IReadOnlyList<KeyValuePair<string, string>> Metadata(ulong callId) => KvOut(callId, attributes: false);

    /// <summary>The caller's attributes as Gate 3 saw them.</summary>
    public IReadOnlyList<KeyValuePair<string, string>> Attributes(ulong callId) => KvOut(callId, attributes: true);

    private unsafe IReadOnlyList<KeyValuePair<string, string>> KvOut(ulong callId, bool attributes)
    {
        if (_disposed) return Array.Empty<KeyValuePair<string, string>>();
        byte* ptr = null;
        uint len = 0;
        ulong buffer = 0;
        int r = attributes
            ? Native.aster_dispatch_attributes(_runtime, _handle, callId, &ptr, &len, &buffer)
            : Native.aster_dispatch_metadata(_runtime, _handle, callId, &ptr, &len, &buffer);
        if (r == (int)Status.NotFound) return Array.Empty<KeyValuePair<string, string>>();
        if (r != 0) throw IrohException.FromStatus(r, attributes ? "aster_dispatch_attributes" : "aster_dispatch_metadata");
        try { return KvList.Decode(new ReadOnlySpan<byte>(ptr, (int)len)); }
        finally { BufferRelease(buffer); }
    }

    /// <summary>Attribute bridge (design §2.6): what Gate 3 sees for <paramref name="peer"/>; empty clears.</summary>
    public unsafe void SetPeerAttributes(string peer, IEnumerable<KeyValuePair<string, string>>? attributes)
    {
        byte[] p = Encoding.UTF8.GetBytes(peer);
        byte[] kv = attributes is null ? Array.Empty<byte>() : KvList.Encode(attributes);
        fixed (byte* pp = p)
        fixed (byte* kp = kv)
            Native.Check(Native.aster_server_set_peer_attributes(_handle, pp, (uint)p.Length, kv.Length == 0 ? null : kp, (uint)kv.Length),
                "aster_server_set_peer_attributes");
    }

    /// <summary>Stops serving: queued calls are abandoned, waiters fail. Call after the poll loop stopped.</summary>
    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        foreach (var kv in _capacityWaiters)
            if (_capacityWaiters.TryRemove(kv))
                kv.Value.TrySetResult(false);
        Native.aster_server_destroy(_runtime, _handle);
    }
}

/// <summary>
/// The C ABI k/v list (metadata, attributes, <c>aster_call_options_t.metadata</c>): <c>u32 LE
/// count</c>, then per entry <c>u32 LE len</c> + key, <c>u32 LE len</c> + value (UTF-8); empty = none.
/// </summary>
internal static class KvList
{
    public static byte[] Encode(IEnumerable<KeyValuePair<string, string>> entries)
    {
        var list = entries as IReadOnlyCollection<KeyValuePair<string, string>> ?? new List<KeyValuePair<string, string>>(entries);
        if (list.Count == 0) return Array.Empty<byte>();
        int size = 4;
        foreach (var kv in list)
            size += 8 + Encoding.UTF8.GetByteCount(kv.Key) + Encoding.UTF8.GetByteCount(kv.Value ?? "");
        var buf = new byte[size];
        var span = buf.AsSpan();
        System.Buffers.Binary.BinaryPrimitives.WriteUInt32LittleEndian(span, (uint)list.Count);
        int o = 4;
        foreach (var kv in list)
        {
            o += Write(span[o..], kv.Key);
            o += Write(span[o..], kv.Value ?? "");
        }
        return buf;

        static int Write(Span<byte> dst, string s)
        {
            int n = Encoding.UTF8.GetBytes(s, dst[4..]);
            System.Buffers.Binary.BinaryPrimitives.WriteUInt32LittleEndian(dst, (uint)n);
            return 4 + n;
        }
    }

    public static IReadOnlyList<KeyValuePair<string, string>> Decode(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length < 4) return Array.Empty<KeyValuePair<string, string>>();
        uint count = System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(bytes);
        var result = new List<KeyValuePair<string, string>>((int)Math.Min(count, 1024));
        int o = 4;
        for (uint i = 0; i < count; i++)
        {
            string k = Read(bytes, ref o);
            string v = Read(bytes, ref o);
            result.Add(new KeyValuePair<string, string>(k, v));
        }
        return result;

        static string Read(ReadOnlySpan<byte> b, ref int o)
        {
            int n = (int)System.Buffers.Binary.BinaryPrimitives.ReadUInt32LittleEndian(b[o..]);
            string s = Encoding.UTF8.GetString(b.Slice(o + 4, n));
            o += 4 + n;
            return s;
        }
    }
}
