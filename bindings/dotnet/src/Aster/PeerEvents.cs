using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading;
using System.Threading.Channels;
using System.Threading.Tasks;

namespace Aster;

/// <summary>Who a connected peer is, as verified when its first connection was admitted.</summary>
/// <param name="EndpointId">The peer's verified endpoint id.</param>
/// <param name="Principal">The principal its credential names; null unless the peer has a live enrollment session.</param>
/// <param name="Roles">The roles its credential grants (sorted); empty without enrollment.</param>
public sealed record PeerIdentity(string EndpointId, string? Principal, IReadOnlyList<string> Roles);

/// <summary>A peer that was connected when <see cref="AsterServer.PeerEventsAsync"/> was called.</summary>
/// <param name="Peer">The peer's endpoint id.</param>
/// <param name="Alpn">The ALPN of its first admitted connection (<c>aster/1</c>).</param>
/// <param name="Connections">How many admitted connections it has.</param>
/// <param name="Identity">Who it is.</param>
public sealed record PeerSnapshot(string Peer, string Alpn, int Connections, PeerIdentity Identity);

/// <summary>Why a peer's last connection ended.</summary>
public enum DisconnectReason
{
    /// <summary>The peer, or this server, closed it normally.</summary>
    Closed,
    /// <summary>It hit its idle timeout.</summary>
    TimedOut,
    /// <summary>The peer's credential expired or was revoked.</summary>
    Revoked,
    /// <summary>The server's drain closed it (<see cref="AsterServer.DrainCloseCode"/>).</summary>
    Drained,
    /// <summary>A transport error or reset.</summary>
    Error,
}

/// <summary>One change in which peers are connected. Per peer, not per connection.</summary>
public abstract record PeerEvent;

/// <summary>A peer's first admitted connection opened.</summary>
public sealed record PeerConnected(string Peer, string Alpn, PeerIdentity Identity) : PeerEvent;

/// <summary>A peer's last connection ended.</summary>
public sealed record PeerDisconnected(string Peer, DisconnectReason Reason) : PeerEvent;

/// <summary>
/// The stream fell behind its bounded queue and <paramref name="Missed"/> events were dropped. The consumer's
/// state is stale: dispose the stream, call <see cref="AsterServer.PeerEventsAsync"/> again and rebuild from the
/// new snapshot.
/// </summary>
public sealed record PeerLagged(ulong Missed) : PeerEvent;

/// <summary>
/// The peers connected now plus every connect and disconnect after them, taken together so a peer connecting
/// during the call is in <see cref="Snapshot"/> or in the stream, never both and never neither. Dispose it when done;
/// disposing wakes the pending read, which then ends.
/// </summary>
public sealed class PeerEventStream : IAsyncEnumerable<PeerEvent>, IAsyncDisposable
{
    private static readonly TimeSpan PollSlice = TimeSpan.FromMilliseconds(100);

    private readonly ulong _runtime;
    private readonly ulong _receiver;
    private readonly Action<PeerEventStream> _onDispose;
    private readonly Channel<PeerEvent> _channel = Channel.CreateBounded<PeerEvent>(
        new BoundedChannelOptions(1) { SingleWriter = true, FullMode = BoundedChannelFullMode.Wait });
    private readonly CancellationTokenSource _stop = new();
    private readonly Thread _thread;
    private CancellationTokenRegistration _lifetime;
    private int _disposed;

    /// <summary>The peers connected when the stream was opened.</summary>
    public IReadOnlyList<PeerSnapshot> Snapshot { get; }

    internal PeerEventStream(ulong runtime, ulong receiver, IReadOnlyList<PeerSnapshot> snapshot, Action<PeerEventStream> onDispose,
        CancellationToken lifetime)
    {
        _runtime = runtime;
        _receiver = receiver;
        _onDispose = onDispose;
        Snapshot = snapshot;
        // A dedicated thread, like the server's poll thread: the native poll blocks, so it never takes a pool thread.
        _thread = new Thread(PollLoop) { Name = "aster-peer-events", IsBackground = true };
        _thread.Start();
        if (lifetime.CanBeCanceled) _lifetime = lifetime.UnsafeRegister(static s => _ = ((PeerEventStream)s!).DisposeAsync().AsTask(), this);
    }

    /// <summary>
    /// Events in order, until the stream is disposed or the server ends. Cancelling <paramref name="cancellationToken"/>
    /// ends this enumeration only; the receiver stays open until <see cref="DisposeAsync"/>.
    /// </summary>
    public IAsyncEnumerable<PeerEvent> ReadAllAsync(CancellationToken cancellationToken = default)
        => _channel.Reader.ReadAllAsync(cancellationToken);

    /// <inheritdoc/>
    public IAsyncEnumerator<PeerEvent> GetAsyncEnumerator(CancellationToken cancellationToken = default)
        => ReadAllAsync(cancellationToken).GetAsyncEnumerator(cancellationToken);

    private unsafe void PollLoop()
    {
        try
        {
            while (!_stop.IsCancellationRequested)
            {
                Bytes output = default;
                int status = Native.aster_peer_events_poll(_runtime, _receiver, (uint)PollSlice.TotalMilliseconds, &output);
                if (status == (int)Status.Timeout) continue;
                if (status == (int)Status.NotFound || status == (int)Status.AlreadyClosed) break;
                if (status != 0) throw IrohException.FromStatus(status, "aster_peer_events_poll");
                var ev = Decode(NativeScope.TakeOwned(ref output));
                // A full channel (one event) blocks this dedicated thread, so a slow consumer backs up into the
                // bounded native queue and gets PeerLagged, rather than growing memory here.
                _channel.Writer.WriteAsync(ev, _stop.Token).AsTask().GetAwaiter().GetResult();
            }
            _channel.Writer.TryComplete();
        }
        catch (OperationCanceledException) { _channel.Writer.TryComplete(); }
        catch (Exception e) { _channel.Writer.TryComplete(e); }
    }

    private sealed record IdentityWire(string EndpointId, string? Principal, List<string>? Roles)
    {
        public PeerIdentity ToIdentity() => new(EndpointId, Principal, Roles ?? []);
    }

    private sealed record EventWire(string Type, string? Peer, string? Alpn, IdentityWire? Identity, string? Reason, ulong Missed);

    private sealed record SnapshotWire(string Peer, string Alpn, int Connections, IdentityWire Identity);

    internal static List<PeerSnapshot> DecodeSnapshot(byte[] json)
    {
        try
        {
            var wire = JsonSerializer.Deserialize<List<SnapshotWire>>(json, GatewayIdentity.Json) ?? [];
            return wire.ConvertAll(p => new PeerSnapshot(p.Peer, p.Alpn, p.Connections, p.Identity.ToIdentity()));
        }
        catch (JsonException e) { throw new IrohException(Status.Internal, $"aster_server_peer_events: {e.Message}"); }
    }

    private static PeerEvent Decode(byte[] json)
    {
        EventWire? wire;
        try { wire = JsonSerializer.Deserialize<EventWire>(json, GatewayIdentity.Json); }
        catch (JsonException e) { throw new IrohException(Status.Internal, $"aster_peer_events_poll: {e.Message}"); }
        return wire?.Type switch
        {
            "connected" => new PeerConnected(wire.Peer ?? "", wire.Alpn ?? "", wire.Identity?.ToIdentity() ?? new PeerIdentity(wire.Peer ?? "", null, [])),
            "disconnected" => new PeerDisconnected(wire.Peer ?? "", wire.Reason switch
            {
                "closed" => DisconnectReason.Closed,
                "timed_out" => DisconnectReason.TimedOut,
                "revoked" => DisconnectReason.Revoked,
                "drained" => DisconnectReason.Drained,
                "error" => DisconnectReason.Error,
                _ => throw new IrohException(Status.Internal, $"aster_peer_events_poll: unknown disconnect reason '{wire.Reason}'"),
            }),
            "lagged" => new PeerLagged(wire.Missed),
            _ => throw new IrohException(Status.Internal, $"aster_peer_events_poll: unknown event type '{wire?.Type}'"),
        };
    }

    /// <summary>Releases the receiver, wakes the pending read and ends the enumeration.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _onDispose(this);
        await _lifetime.DisposeAsync().ConfigureAwait(false);
        _stop.Cancel();
        // Release wakes a poll blocked in native code (NOT_FOUND); NOT_FOUND here just means the server already ended.
        _ = Native.aster_peer_events_release(_receiver);
        await Task.Run(() => _thread.Join(TimeSpan.FromSeconds(5))).ConfigureAwait(false);
        _channel.Writer.TryComplete();
        _stop.Dispose();
    }
}

public sealed partial class AsterServer
{
    private readonly ConcurrentDictionary<PeerEventStream, byte> _peerStreams = new();

    /// <summary>
    /// The peers connected now and a stream of every connect and disconnect after them, taken together
    /// (Rust <c>AsterServer::peer_events</c>). Events are per peer: <see cref="PeerConnected"/> on a peer's first
    /// admitted <c>aster/1</c> connection (past the node's gates, so a peer enrollment or a drain refuses never
    /// appears), <see cref="PeerDisconnected"/> when its last one ends. Whether a connected peer serves what you need
    /// is answered by reading its <c>aster.services</c> NodeInfo attribute.
    /// <para>
    /// A consumer that falls behind the bounded queue (1024 events) gets one <see cref="PeerLagged"/>; its recovery
    /// is to open a new stream. Connections are never held up by a slow consumer. Cancelling
    /// <paramref name="cancellationToken"/> after this returns disposes the stream and releases the receiver; the
    /// server's own disposal does too.
    /// </para>
    /// </summary>
    /// <exception cref="ObjectDisposedException">The server is disposed.</exception>
    public unsafe Task<PeerEventStream> PeerEventsAsync(CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
        ulong receiver = 0;
        Bytes output = default;
        Native.Check(Native.aster_server_peer_events(_dispatch.NativeHandle, &receiver, &output), "aster_server_peer_events");
        List<PeerSnapshot> snapshot;
        try { snapshot = PeerEventStream.DecodeSnapshot(NativeScope.TakeOwned(ref output)); }
        catch (Exception)
        {
            _ = Native.aster_peer_events_release(receiver);
            throw;
        }
        var stream = new PeerEventStream(_dispatch.NativeRuntime, receiver, snapshot, s => _peerStreams.TryRemove(s, out _), cancellationToken);
        _peerStreams[stream] = 0;
        return Task.FromResult(stream);
    }

    private async ValueTask DisposePeerStreamsAsync()
    {
        foreach (var stream in _peerStreams.Keys) await stream.DisposeAsync().ConfigureAwait(false);
    }
}
