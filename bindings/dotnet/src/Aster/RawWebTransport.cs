using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Serialization;
using Aster.Upcall;

namespace Aster;

/// <summary>Separate exact-route raw WebTransport listener. Zero limits select bounded native defaults.</summary>
public sealed record RawWTOptions
{
    public string BindAddress { get; init; } = "127.0.0.1:0";
    public byte[] CertificatePem { get; init; } = [];
    public byte[] PrivateKeyPem { get; init; } = [];
    public string Path { get; init; } = "/raw";
    public uint MaxSessions { get; init; }
    public uint MaxStreams { get; init; }
    public uint MaxDatagrams { get; init; }
    public uint MaxDatagramBytes { get; init; }
    public uint MaxReadBytes { get; init; }
    public uint MaxPendingAuth { get; init; }
    public uint MaxAuthPayloadBytes { get; init; }
}

internal delegate int RawWTSubmit(out ulong operation);
internal delegate int RawWTSnapshot(out Bytes snapshot);

// Retain through terminal collection and owner construction, never cancel only
// the managed Task: a committed native completion can carry a live handle.
internal sealed class RawWTOwner(Runtime runtime, ulong handle, Func<int> free) : IDisposable
{
    internal readonly Runtime Runtime = runtime.Retain();
    internal readonly ulong Handle = handle;
    private readonly object _gate = new();
    private bool _disposed;
    internal Runtime Acquire()
    {
        lock (_gate) { ObjectDisposedException.ThrowIf(_disposed, this); return Runtime.Retain(); }
    }
    internal async Task<T> Run<T>(RawWTSubmit call, uint kind, Func<Event, byte[], T> decode, CancellationToken ct)
    {
        ct.ThrowIfCancellationRequested();
        var runtime = Acquire();
        try
        {
            Native.Check(call(out ulong op), "raw WebTransport submit");
            var ev = await DocEventReceiver.AwaitTerminal(runtime, op, ct).ConfigureAwait(false);
            if (ev.kind != kind || ev.status != 0) throw EventError.From(runtime, ev, "raw WebTransport completion");
            return decode(ev, EventError.TakePayload(runtime, ev));
        }
        finally { runtime.Release(); }
    }
    internal T Sync<T>(Func<T> call)
    {
        var runtime = Acquire();
        try { return call(); } finally { runtime.Release(); }
    }
    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed) return;
            int status = free();
            if (status != (int)Status.NotFound && status != (int)Status.AlreadyClosed) Native.Check(status, "raw WebTransport free");
            _disposed = true;
            Runtime.Release();
        }
    }
}

internal static class RawWTJson
{
    private static readonly JsonSerializerOptions Options = new() { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower };
    internal static T Decode<T>(byte[] bytes)
    {
        using var document = JsonDocument.Parse(bytes);
        if (!document.RootElement.TryGetProperty("version", out var version) || version.GetInt32() != 1)
            throw new InvalidDataException("unsupported raw WebTransport snapshot version");
        return JsonSerializer.Deserialize<T>(bytes, Options) ?? throw new InvalidDataException("empty raw WebTransport snapshot");
    }
    internal static T Read<T>(RawWTSnapshot read)
    {
        Bytes span = default;
        try { Native.Check(read(out span), "raw WebTransport snapshot"); return Decode<T>(NativeScope.Read(span)); }
        finally { if (span.ptr != IntPtr.Zero) Native.aster_string_release(span.ptr, span.len); }
    }
}

/// <summary>Owned TCP/QUIC raw listener, separate from RPC and arbitrary foreign HTTP routes.</summary>
public sealed class RawWTListener : IDisposable, IAsyncDisposable
{
    private readonly RawWTOwner _owner;
    private readonly Upcalls? _upcalls;
    private readonly SemaphoreSlim _stop = new(1, 1);
    private bool _closed;
    private RawWTListener(Runtime runtime, ulong handle, Upcalls? upcalls)
        => (_owner, _upcalls) = (new RawWTOwner(runtime, handle, () => 0), upcalls);
    public string TcpAddress { get; private set; } = "";
    public string QuicAddress { get; private set; } = "";

    /// <summary>Authenticate before upgrade using a live same-runtime typed authenticator.
    /// Metadata includes aster.webtransport.path and the unnormalized query. Peer addresses are hints, not Iroh identities.</summary>
    public static Task<RawWTListener> StartAsync(Runtime runtime, RawWTOptions options, Upcalls upcalls, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(upcalls);
        upcalls.RequireRpcAuthenticator(runtime);
        return StartCore(runtime, options, upcalls, ct);
    }
    /// <summary>Explicit unauthenticated serving. Gate media on application PoP if required.</summary>
    public static Task<RawWTListener> StartPublicAsync(Runtime runtime, RawWTOptions options, CancellationToken ct = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        if (options.MaxPendingAuth != 0 || options.MaxAuthPayloadBytes != 0) throw new ArgumentException("public listeners do not accept authentication limits", nameof(options));
        return StartCore(runtime, options, null, ct);
    }
    private static async Task<RawWTListener> StartCore(Runtime runtime, RawWTOptions options, Upcalls? upcalls, CancellationToken ct)
    {
        ArgumentNullException.ThrowIfNull(runtime); ArgumentNullException.ThrowIfNull(options); ct.ThrowIfCancellationRequested();
        var snapshot = options with { CertificatePem = options.CertificatePem.ToArray(), PrivateKeyPem = options.PrivateKeyPem.ToArray() };
        runtime.Retain();
        try
        {
            var handle = await Task.Run(() =>
            {
                ct.ThrowIfCancellationRequested();
                using var scope = new NativeScope();
                var config = new RawWTConfigNative
                {
                    struct_size = (uint)Marshal.SizeOf<RawWTConfigNative>(),
                    bind_addr = scope.Utf8(snapshot.BindAddress),
                    certificate_pem = scope.Bytes(snapshot.CertificatePem),
                    private_key_pem = scope.Bytes(snapshot.PrivateKeyPem),
                    path = scope.Utf8(snapshot.Path),
                    max_sessions = snapshot.MaxSessions,
                    max_streams = snapshot.MaxStreams,
                    max_datagrams = snapshot.MaxDatagrams,
                    max_datagram_bytes = snapshot.MaxDatagramBytes,
                    max_read_bytes = snapshot.MaxReadBytes,
                    max_pending_auth = snapshot.MaxPendingAuth,
                    max_auth_payload_bytes = snapshot.MaxAuthPayloadBytes,
                };
                int status = upcalls is null
                    ? Native.aster_raw_wt_listener_start_public(runtime.Handle, in config, out ulong h)
                    : Native.aster_raw_wt_listener_start(runtime.Handle, upcalls.NativeHandle, in config, out h);
                Native.Check(status, "raw WebTransport start"); return h;
            }, CancellationToken.None).ConfigureAwait(false);
            var listener = new RawWTListener(runtime, handle, upcalls);
            try
            {
                ct.ThrowIfCancellationRequested();
                var address = RawWTJson.Read<Addresses>((out Bytes bytes) => Native.aster_raw_wt_listener_addr(runtime.Handle, handle, out bytes));
                if (string.IsNullOrEmpty(address.TcpAddr) || string.IsNullOrEmpty(address.QuicAddr)) throw new InvalidDataException("missing raw listener address");
                listener.TcpAddress = address.TcpAddr; listener.QuicAddress = address.QuicAddr;
                ct.ThrowIfCancellationRequested(); return listener;
            }
            catch (Exception cause)
            {
                try { await listener.DisposeAsync().ConfigureAwait(false); }
                catch (Exception cleanup) { throw new RawWTStartException(listener, cause, cleanup); }
                throw;
            }
        }
        finally { runtime.Release(); }
    }
    private sealed record Addresses(string TcpAddr, string QuicAddr);
    public Task<RawWTSession> AcceptAsync(CancellationToken ct = default)
        => _owner.Run((out ulong op) => Native.aster_raw_wt_listener_accept(_owner.Runtime.Handle, _owner.Handle, 0, out op), (uint)EventKind.RawWTSession,
            (ev, _) => new RawWTSession(_owner.Runtime, ev.handle), ct);
    /// <summary>Seal admission and join application/protocol cleanup. Grace is not a socket-join deadline.</summary>
    public async Task StopAsync(TimeSpan grace, CancellationToken ct = default)
    {
        if (grace < TimeSpan.Zero) throw new ArgumentOutOfRangeException(nameof(grace));
        await _stop.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            if (_closed) return; ct.ThrowIfCancellationRequested();
            uint milliseconds = (uint)Math.Min(uint.MaxValue, Math.Ceiling(grace.TotalMilliseconds));
            await Task.Run(() =>
            {
                int status = Native.aster_raw_wt_listener_stop(_owner.Runtime.Handle, _owner.Handle, milliseconds);
                if (status != (int)Status.NotFound && status != (int)Status.AlreadyClosed) Native.Check(status, "raw WebTransport stop");
            }, CancellationToken.None).ConfigureAwait(false);
            _closed = true; _owner.Dispose();
        }
        finally { _stop.Release(); }
    }
    public ValueTask DisposeAsync() => new(StopAsync(TimeSpan.Zero));
    public void Dispose() => DisposeAsync().AsTask().GetAwaiter().GetResult();
}

/// <summary>Startup cleanup failed; dispose Listener to retry owned cleanup.</summary>
public sealed class RawWTStartException : Exception
{
    internal RawWTStartException(RawWTListener listener, Exception cause, Exception cleanup)
        : base("raw WT startup cleanup failed; dispose Listener to retry", new AggregateException(cause, cleanup)) => Listener = listener;
    public RawWTListener Listener { get; }
}

public sealed record RawWTSessionInfo(int Version, ulong ConnectionId, ulong SessionId, string PeerAddress, bool Closed, string? Principal, Dictionary<string, string> Attributes);
public sealed record RawWTCloseInfo(int Version, string Kind, ulong? Code, string Reason);
public sealed record RawWTStats(int Version, ulong RttMicros, [property: JsonPropertyName("cwnd")] ulong CongestionWindow, ushort Mtu,
    ulong SentPackets, ulong LostPackets, ulong LostBytes, ulong CongestionEvents, ulong UdpTxBytes, ulong UdpTxPackets,
    ulong UdpRxBytes, ulong UdpRxPackets, ulong PingTx, ulong AckRx);

/// <summary>Owned raw session. CloseConnection closes its underlying QUIC connection, including other sessions on it.</summary>
public sealed class RawWTSession : IDisposable, IAsyncDisposable
{
    private readonly RawWTOwner _owner;
    internal RawWTSession(Runtime runtime, ulong handle) => _owner = new(runtime, handle, () => Native.aster_raw_wt_session_free(runtime.Handle, handle));
    public RawWTSessionInfo Info => _owner.Sync(() => RawWTJson.Read<RawWTSessionInfo>((out Bytes bytes) => Native.aster_raw_wt_session_info(_owner.Runtime.Handle, _owner.Handle, out bytes)));
    public RawWTStats Stats => _owner.Sync(() => RawWTJson.Read<RawWTStats>((out Bytes bytes) => Native.aster_raw_wt_session_stats(_owner.Runtime.Handle, _owner.Handle, out bytes)));
    public Task<RawWTCloseInfo> ClosedAsync(CancellationToken ct = default)
        => _owner.Run((out ulong op) => Native.aster_raw_wt_session_closed(_owner.Runtime.Handle, _owner.Handle, 0, out op), (uint)EventKind.RawWTClosed, (_, bytes) => RawWTJson.Decode<RawWTCloseInfo>(bytes), ct);
    public void CloseConnection(uint code = 0, ReadOnlySpan<byte> reason = default)
    {
        using var scope = new NativeScope(); var bytes = scope.Bytes(reason);
        _owner.Sync(() => { Native.Check(Native.aster_raw_wt_session_close(_owner.Runtime.Handle, _owner.Handle, code, bytes), "raw WT close"); return 0; });
    }
    public ulong? MaxDatagramSize => _owner.Sync<ulong?>(() =>
    {
        Native.Check(Native.aster_raw_wt_session_max_datagram_size(_owner.Runtime.Handle, _owner.Handle, out ulong size, out uint some), "raw WT datagram limit");
        return some != 0 ? size : null;
    });
    public void SendDatagram(ReadOnlySpan<byte> payload)
    {
        using var scope = new NativeScope(); var bytes = scope.Bytes(payload);
        _owner.Sync(() => { Native.Check(Native.aster_raw_wt_session_send_datagram(_owner.Runtime.Handle, _owner.Handle, bytes), "raw WT datagram"); return 0; });
    }
    public Task<byte[]> ReceiveDatagramAsync(CancellationToken ct = default)
        => _owner.Run((out ulong op) => Native.aster_raw_wt_session_receive_datagram(_owner.Runtime.Handle, _owner.Handle, 0, out op), (uint)EventKind.RawWTDatagram, (_, bytes) => bytes, ct);
    private Task<RawWTStream> Stream(RawWTSubmit call, CancellationToken ct)
        => _owner.Run(call, (uint)EventKind.RawWTStream, (ev, _) =>
        {
            if (ev.flags == 0 || (ev.flags & ~3u) != 0) { Native.aster_raw_wt_stream_free(_owner.Runtime.Handle, ev.handle); throw new InvalidDataException("invalid raw WT stream directions"); }
            return new RawWTStream(_owner.Runtime, ev.handle, (ev.flags & 1) != 0, (ev.flags & 2) != 0);
        }, ct);
    public Task<RawWTStream> OpenBiAsync(CancellationToken ct = default) => Stream((out ulong op) => Native.aster_raw_wt_session_open_bi(_owner.Runtime.Handle, _owner.Handle, 0, out op), ct);
    public Task<RawWTStream> OpenUniAsync(CancellationToken ct = default) => Stream((out ulong op) => Native.aster_raw_wt_session_open_uni(_owner.Runtime.Handle, _owner.Handle, 0, out op), ct);
    public Task<RawWTStream> AcceptBiAsync(CancellationToken ct = default) => Stream((out ulong op) => Native.aster_raw_wt_session_accept_bi(_owner.Runtime.Handle, _owner.Handle, 0, out op), ct);
    public Task<RawWTStream> AcceptUniAsync(CancellationToken ct = default) => Stream((out ulong op) => Native.aster_raw_wt_session_accept_uni(_owner.Runtime.Handle, _owner.Handle, 0, out op), ct);
    public void Dispose() => _owner.Dispose();
    public ValueTask DisposeAsync() { Dispose(); return ValueTask.CompletedTask; }
}

/// <summary>One unframed stream. Operations serialize per direction. Dispose resets/stops unfinished directions.</summary>
public sealed class RawWTStream : IDisposable, IAsyncDisposable
{
    private readonly RawWTOwner _owner;
    internal RawWTStream(Runtime runtime, ulong handle, bool send, bool receive)
        => (_owner, CanSend, CanReceive) = (new RawWTOwner(runtime, handle, () => Native.aster_raw_wt_stream_free(runtime.Handle, handle)), send, receive);
    public bool CanSend { get; }
    public bool CanReceive { get; }
    /// <summary>Read bounded bytes; null means EOF. Cancellation leaves unconsumed bytes for the next read.</summary>
    public Task<byte[]?> ReadAsync(uint maxBytes = 65536, CancellationToken ct = default)
        => _owner.Run<byte[]?>((out ulong op) => Native.aster_raw_wt_stream_read(_owner.Runtime.Handle, _owner.Handle, maxBytes, 0, out op), (uint)EventKind.RawWTRead, (ev, bytes) => (ev.flags & 1) != 0 ? null : bytes, ct);
    private Task Unit(RawWTSubmit call, CancellationToken ct) => _owner.Run(call, (uint)EventKind.UnitResult, (_, _) => 0, ct);
    /// <summary>Write all bytes; cancellation can leave a transmitted prefix. Never retry automatically.</summary>
    public async Task WriteAsync(ReadOnlyMemory<byte> bytes, CancellationToken ct = default)
    {
        if (bytes.Length > 16 * 1024 * 1024) throw new ArgumentOutOfRangeException(nameof(bytes), "raw WT write exceeds the 16 MiB ABI limit");
        using var scope = new NativeScope(); var span = scope.Bytes(bytes.Span);
        await Unit((out ulong op) => Native.aster_raw_wt_stream_write(_owner.Runtime.Handle, _owner.Handle, span, 0, out op), ct).ConfigureAwait(false);
    }
    /// <summary>Queue FIN without releasing the send owner or proving receipt.</summary>
    public Task FinishAsync(CancellationToken ct = default) => Unit((out ulong op) => Native.aster_raw_wt_stream_finish(_owner.Runtime.Handle, _owner.Handle, 0, out op), ct);
    public Task ResetAsync(uint code = 0, CancellationToken ct = default) => Unit((out ulong op) => Native.aster_raw_wt_stream_reset(_owner.Runtime.Handle, _owner.Handle, code, 0, out op), ct);
    public Task StopAsync(uint code = 0, CancellationToken ct = default) => Unit((out ulong op) => Native.aster_raw_wt_stream_stop(_owner.Runtime.Handle, _owner.Handle, code, 0, out op), ct);
    /// <summary>Signed Quinn priority: larger values are scheduled sooner.</summary>
    public Task SetPriorityAsync(int priority, CancellationToken ct = default) => Unit((out ulong op) => Native.aster_raw_wt_stream_set_priority(_owner.Runtime.Handle, _owner.Handle, priority, 0, out op), ct);
    public Task<int?> PriorityAsync(CancellationToken ct = default)
        => _owner.Run((out ulong op) => Native.aster_raw_wt_stream_priority(_owner.Runtime.Handle, _owner.Handle, 0, out op), (uint)EventKind.BytesResult, (ev, bytes) => DecodePriority(ev.flags, bytes), ct);
    internal static int? DecodePriority(uint flags, byte[] bytes)
    {
        if (flags == 0 && bytes.Length == 0) return null;
        if (flags != 1 || bytes.Length != 4) throw new InvalidDataException("invalid raw WT priority payload");
        return BinaryPrimitives.ReadInt32LittleEndian(bytes);
    }
    /// <summary>True only for bytes+FIN transport ACK. False means reset/loss; cancellation throws.</summary>
    public Task<bool> TransportAckedAsync(CancellationToken ct = default)
        => _owner.Run((out ulong op) => Native.aster_raw_wt_stream_transport_acked(_owner.Runtime.Handle, _owner.Handle, 0, out op), (uint)EventKind.RawWTAcked, (ev, _) => (ev.flags & 1) != 0, ct);
    public void Dispose() => _owner.Dispose();
    public ValueTask DisposeAsync() { Dispose(); return ValueTask.CompletedTask; }
}
