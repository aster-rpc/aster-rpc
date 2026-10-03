using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// An event delivered by <see cref="Reactor.Poll"/>: either an inbound RPC call
/// (<see cref="ReactorEventKind.Call"/>) or a connection-closed notification
/// (<see cref="ReactorEventKind.ConnectionClosed"/>). Response-capacity events are
/// consumed inside <see cref="Reactor"/> and never surface here.
/// </summary>
public sealed class ReactorCall
{
    public ReactorEventKind Kind { get; init; }

    /// <summary>Reactor call id for submit correlation. Zero on ConnectionClosed.</summary>
    public ulong CallId { get; init; }

    /// <summary>
    /// Reactor-assigned connection id. Key per-session state on
    /// (ConnectionId, StreamHeader.sessionId); a ConnectionClosed event with the
    /// same id means that state can be dropped.
    /// </summary>
    public ulong ConnectionId { get; init; }

    /// <summary>QUIC bi-stream id. Zero on ConnectionClosed. Do not key session state on it.</summary>
    public ulong StreamId { get; init; }

    /// <summary>
    /// Borrowed connection handle; owned by the reactor. Never pass it to a free function.
    /// </summary>
    public ulong ConnectionHandle { get; init; }

    public byte[] Header { get; init; } = Array.Empty<byte>();
    public byte HeaderFlags { get; init; }
    public byte[] Request { get; init; } = Array.Empty<byte>();
    public byte RequestFlags { get; init; }
    public string PeerId { get; init; } = string.Empty;
}

/// <summary>
/// Response to an Aster RPC call.
/// </summary>
public sealed class ReactorResponse
{
    public byte[] ResponseFrame { get; init; } = Array.Empty<byte>();
    public byte[] TrailerFrame { get; init; } = Array.Empty<byte>();

    public static ReactorResponse Of(byte[] response) => new() { ResponseFrame = response };
    public static ReactorResponse Of(byte[] response, byte[] trailer) => new() { ResponseFrame = response, TrailerFrame = trailer };
}

/// <summary>
/// Wraps the aster_reactor_* C API for RPC call delivery via SPSC ring buffer.
///
/// Threading contract (same as the Java binding): exactly one thread calls
/// <see cref="Poll"/>; any thread may call <see cref="Submit"/> /
/// <see cref="SubmitAsync"/>, with at most one producer per call id. A submit that
/// hits backpressure (QUEUE_FULL) waits for a RESPONSE_WRITABLE notification that
/// only <see cref="Poll"/> can deliver, so submits must not run on the poll thread
/// and the poll loop must keep running while submits are pending.
/// </summary>
public sealed class Reactor : IDisposable
{
    /// <summary>sizeof(aster_reactor_call_t) on 64-bit targets (ffi/src/layout.rs).</summary>
    internal const int CallStructSize = 112;

    private readonly ulong _runtimeHandle;
    private readonly ulong _handle;
    private volatile bool _disposed;

    private ReactorCallNative[] _pollBuffer = Array.Empty<ReactorCallNative>();
    private int _pollThreadId = -1;

    // call_id -> waiter for an asynchronous response-capacity reservation.
    private readonly ConcurrentDictionary<ulong, TaskCompletionSource> _capacityWaiters = new();

    private static long s_queueFullHits;

    /// <summary>Diagnostic: total QUEUE_FULL results observed in this process.</summary>
    public static long QueueFullHits => Interlocked.Read(ref s_queueFullHits);

    public Reactor(ulong runtimeHandle, ulong nodeHandle, uint ringCapacity = 256)
    {
        _runtimeHandle = runtimeHandle;
        int r = Native.aster_reactor_create(runtimeHandle, nodeHandle, ringCapacity, out _handle);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_reactor_create");
    }

    /// <summary>
    /// Polls the reactor. Returns Call and ConnectionClosed events with their data
    /// copied into managed memory; every native buffer they referenced is released
    /// before returning. Response-capacity events (RESPONSE_WRITABLE /
    /// RESPONSE_CLOSED) are routed to the pending <see cref="Submit"/> waiters and
    /// are not returned.
    /// </summary>
    public unsafe ReactorCall[] Poll(int maxCalls = 32, uint timeoutMs = 100)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (maxCalls <= 0) throw new ArgumentOutOfRangeException(nameof(maxCalls));
        _pollThreadId = Environment.CurrentManagedThreadId;

        if (_pollBuffer.Length < maxCalls)
            _pollBuffer = new ReactorCallNative[maxCalls];

        uint n;
        fixed (ReactorCallNative* buf = _pollBuffer)
        {
            n = Native.aster_reactor_poll(_runtimeHandle, _handle, buf, (uint)maxCalls, timeoutMs);
        }
        if (n == 0) return Array.Empty<ReactorCall>();

        var result = new List<ReactorCall>((int)n);
        for (int i = 0; i < (int)n; i++)
        {
            ref readonly ReactorCallNative c = ref _pollBuffer[i];
            switch ((ReactorEventKind)c.event_kind)
            {
                case ReactorEventKind.Call:
                case ReactorEventKind.ConnectionClosed:
                    try
                    {
                        result.Add(ExtractEvent(in c));
                    }
                    finally
                    {
                        ReleaseBuffers(in c);
                    }
                    break;

                case ReactorEventKind.ResponseWritable:
                case ReactorEventKind.ResponseClosed:
                    // Capacity notifications carry only event_kind + call_id.
                    HandleResponseCapacityEvent((ReactorEventKind)c.event_kind, c.call_id);
                    break;

                default:
                    // Unknown kind from a newer minor ABI: release anything it owns.
                    ReleaseBuffers(in c);
                    break;
            }
        }
        return result.ToArray();
    }

    /// <summary>
    /// Submits a unary response (response frame + trailer) for a call. Blocks the
    /// calling thread while the per-call response channel is full.
    /// </summary>
    public void Submit(ulong callId, ReactorResponse response)
        => SubmitCore(callId, response, CancellationToken.None, sync: true).GetAwaiter().GetResult();

    /// <summary>
    /// Submits a unary response, awaiting (not blocking) while the per-call
    /// response channel is full. Cancelling <paramref name="ct"/> cancels the
    /// native capacity reservation; the response is then not sent.
    /// </summary>
    public Task SubmitAsync(ulong callId, ReactorResponse response, CancellationToken ct = default)
        => SubmitCore(callId, response, ct, sync: false);

    private async Task SubmitCore(ulong callId, ReactorResponse response, CancellationToken ct, bool sync)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        byte[] resp = response.ResponseFrame ?? Array.Empty<byte>();
        byte[] trailer = response.TrailerFrame ?? Array.Empty<byte>();
        ulong encodedLen = (ulong)resp.Length + (ulong)trailer.Length;

        // Pin for the whole retry loop: QUEUE_FULL copies nothing, so the exact
        // same buffers are resubmitted.
        GCHandle respPin = resp.Length > 0 ? GCHandle.Alloc(resp, GCHandleType.Pinned) : default;
        GCHandle trailerPin = trailer.Length > 0 ? GCHandle.Alloc(trailer, GCHandleType.Pinned) : default;
        try
        {
            IntPtr respPtr = respPin.IsAllocated ? respPin.AddrOfPinnedObject() : IntPtr.Zero;
            IntPtr trailerPtr = trailerPin.IsAllocated ? trailerPin.AddrOfPinnedObject() : IntPtr.Zero;
            while (true)
            {
                int r = Native.aster_reactor_submit(_runtimeHandle, _handle, callId,
                    respPtr, (uint)resp.Length, trailerPtr, (uint)trailer.Length);
                if (r != (int)Status.QueueFull)
                {
                    if (r != 0)
                        throw IrohException.FromStatus(r, "aster_reactor_submit");
                    return;
                }
                Interlocked.Increment(ref s_queueFullHits);
                Task wait = BeginResponseCapacityWait(callId, encodedLen, ct);
                if (sync)
                    wait.GetAwaiter().GetResult();
                else
                    await wait.ConfigureAwait(false);
                // The held permit guarantees the retry cannot return QUEUE_FULL.
            }
        }
        finally
        {
            if (respPin.IsAllocated) respPin.Free();
            if (trailerPin.IsAllocated) trailerPin.Free();
        }
    }

    /// <summary>
    /// Asks native code to reserve <paramref name="encodedLen"/> bytes plus one slot
    /// on the call's response channel. The returned task completes when Poll sees
    /// RESPONSE_WRITABLE (or faults on RESPONSE_CLOSED / cancellation / dispose).
    /// </summary>
    private Task BeginResponseCapacityWait(ulong callId, ulong encodedLen, CancellationToken ct)
    {
        if (Environment.CurrentManagedThreadId == _pollThreadId)
            throw new InvalidOperationException(
                "aster_reactor_submit returned QUEUE_FULL on the reactor poll thread; waiting here " +
                "would deadlock because only Poll() delivers RESPONSE_WRITABLE. Submit from another thread.");

        var waiter = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!_capacityWaiters.TryAdd(callId, waiter))
            throw new InvalidOperationException(
                $"concurrent response-capacity waits violate the one-producer contract for call {callId}");

        int status = Native.aster_reactor_reserve_response_capacity(_runtimeHandle, _handle, callId, encodedLen);
        if (status != 0)
        {
            _capacityWaiters.TryRemove(new KeyValuePair<ulong, TaskCompletionSource>(callId, waiter));
            throw IrohException.FromStatus(status, "aster_reactor_reserve_response_capacity");
        }

        if (ct.CanBeCanceled)
        {
            var reg = ct.Register(() =>
            {
                if (_capacityWaiters.TryRemove(new KeyValuePair<ulong, TaskCompletionSource>(callId, waiter)))
                {
                    int cs = Native.aster_reactor_cancel_response_reservation(_runtimeHandle, _handle, callId);
                    if (cs != 0 && cs != (int)Status.NotFound)
                        waiter.TrySetException(IrohException.FromStatus(cs, "aster_reactor_cancel_response_reservation"));
                    else
                        waiter.TrySetCanceled(ct);
                }
            });
            waiter.Task.ContinueWith(static (_, s) => ((CancellationTokenRegistration)s!).Dispose(), reg,
                CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
        }
        return waiter.Task;
    }

    private void HandleResponseCapacityEvent(ReactorEventKind kind, ulong callId)
    {
        if (!_capacityWaiters.TryRemove(callId, out var waiter))
            return; // waiter was cancelled; native side already dropped the reservation
        if (kind == ReactorEventKind.ResponseWritable)
            waiter.TrySetResult();
        else
            waiter.TrySetException(new IrohException(Status.NotFound,
                $"response channel closed for call {callId}"));
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        foreach (var kv in _capacityWaiters)
        {
            if (_capacityWaiters.TryRemove(kv))
                kv.Value.TrySetException(new ObjectDisposedException(nameof(Reactor)));
        }
        Native.aster_reactor_destroy(_runtimeHandle, _handle);
    }

    private void ReleaseBuffers(in ReactorCallNative c)
    {
        if (c.header_buffer != 0) Native.aster_reactor_buffer_release(_runtimeHandle, _handle, c.header_buffer);
        if (c.request_buffer != 0) Native.aster_reactor_buffer_release(_runtimeHandle, _handle, c.request_buffer);
        if (c.peer_buffer != 0) Native.aster_reactor_buffer_release(_runtimeHandle, _handle, c.peer_buffer);
    }

    private static byte[] Copy(IntPtr ptr, uint len)
    {
        if (ptr == IntPtr.Zero || len == 0) return Array.Empty<byte>();
        var b = new byte[len];
        Marshal.Copy(ptr, b, 0, (int)len);
        return b;
    }

    private static ReactorCall ExtractEvent(in ReactorCallNative c)
    {
        byte[] peer = Copy(c.peer_ptr, c.peer_len);
        var kind = (ReactorEventKind)c.event_kind;
        if (kind == ReactorEventKind.ConnectionClosed)
        {
            // Only connection_id, peer_* and connection_handle are populated.
            return new ReactorCall
            {
                Kind = kind,
                ConnectionId = c.connection_id,
                ConnectionHandle = c.connection_handle,
                PeerId = System.Text.Encoding.UTF8.GetString(peer),
            };
        }
        return new ReactorCall
        {
            Kind = kind,
            CallId = c.call_id,
            ConnectionId = c.connection_id,
            StreamId = c.stream_id,
            ConnectionHandle = c.connection_handle,
            Header = Copy(c.header_ptr, c.header_len),
            HeaderFlags = c.header_flags,
            Request = Copy(c.request_ptr, c.request_len),
            RequestFlags = c.request_flags,
            PeerId = System.Text.Encoding.UTF8.GetString(peer),
        };
    }
}
