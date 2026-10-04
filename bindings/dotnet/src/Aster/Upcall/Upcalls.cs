using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Lease;

namespace Aster.Upcall;

/// <summary>What an upcall asks (<c>aster_upcall_kind_t</c>; codes never change).</summary>
public enum UpcallKind : ushort
{
    Cancel = 0,
    LeaseTransition = 1,
    LeaseSnapshot = 2,
    LeaseAuthorize = 3,
    FencedMutate = 4,
    TopologyAdmitted = 16,
    TopologySnapshot = 17,
    PlacementPolicy = 32,
    PlacementProvision = 33,
    ExposeAuthorize = 48,
    ExposeHttp = 49,
    ExposeRoutePolicy = 50,
    AdmissionConnect = 64,
    AdmissionHandshake = 65,
}

/// <summary><c>aster_upcall_status_t</c>: how a binding answers an upcall.</summary>
public enum UpcallStatus : uint
{
    /// <summary>The payload is the kind's UCv1 response.</summary>
    Ok = 0,
    /// <summary>The payload is a UTF-8 message.</summary>
    Error = 1,
    /// <summary>The binding has no handler for the kind.</summary>
    Unimplemented = 2,
}

/// <summary><c>aster_upcall_cancel_reason_t</c>: why Rust stopped waiting for an upcall.</summary>
public enum UpcallCancelReason : byte
{
    None = 0,
    Timeout = 1,
    Abandoned = 2,
    Shutdown = 3,
}

/// <summary>One upcall being handled: its identity, deadline and cancellation.</summary>
public sealed class UpcallContext
{
    private readonly Upcalls _owner;
    private readonly CancellationTokenSource _cts = new();
    private int _answered;
    private int _reason;

    internal UpcallContext(Upcalls owner, ulong invocationId, UpcallKind kind, long deadlineUnixMs)
    {
        _owner = owner;
        InvocationId = invocationId;
        Kind = kind;
        DeadlineUnixMs = deadlineUnixMs;
    }

    public ulong InvocationId { get; }
    public UpcallKind Kind { get; }

    /// <summary>When Rust stops waiting (Unix ms). Informational: Rust enforces the timeout.</summary>
    public long DeadlineUnixMs { get; }

    /// <summary>Cancelled when Rust stops waiting (timeout, abandoned caller) or the table is disposed.</summary>
    public CancellationToken Cancellation => _cts.Token;

    /// <summary>Why <see cref="Cancellation"/> fired (<see cref="UpcallCancelReason.None"/> while it has not).</summary>
    public UpcallCancelReason CancelReason => (UpcallCancelReason)Volatile.Read(ref _reason);

    internal bool TryMarkAnswered() => Interlocked.Exchange(ref _answered, 1) == 0;

    internal void Cancel(UpcallCancelReason reason)
    {
        Interlocked.CompareExchange(ref _reason, (int)reason, 0);
        try { _cts.Cancel(); } catch (ObjectDisposedException) { /* the handler already finished */ }
    }

    internal void Dispose() => _cts.Dispose();

    /// <summary>
    /// Answers <c>OK</c> now, before the handler returns (the HTTP head of a streamed response).
    /// False when the call was already answered or Rust no longer waits. The handler's own return
    /// value is ignored once this succeeded.
    /// </summary>
    public bool TryRespond(byte[] payload)
    {
        if (!TryMarkAnswered()) return false;
        return _owner.Respond(InvocationId, UpcallStatus.Ok, payload);
    }
}

/// <summary>A raw handler: the kind's UCv1 request in, its UCv1 response out (null = empty).</summary>
public delegate ValueTask<byte[]?> UpcallHandler(byte[] request, UpcallContext context);

/// <summary>
/// Topology read-time admission filter, served by upcall (kind <c>TOPOLOGY_ADMITTED</c>, batched):
/// one answer per id, same order. Ids are lowercase 64-hex node ids.
/// </summary>
public interface IAdmittedFilter
{
    ValueTask<IReadOnlyList<bool>> AdmittedAsync(IReadOnlyList<string> nodeIds, CancellationToken cancellationToken);
}

/// <summary>Decides whether a peer may use an exposed service (kind <c>EXPOSE_AUTHORIZE</c>, a decision).</summary>
public interface IExposeAuthorizer
{
    ValueTask<bool> AuthorizeAsync(ExposeAuthorizeRequest request, CancellationToken cancellationToken);
}

/// <summary>Decides which requested routes an edge grants (kind <c>EXPOSE_ROUTE_POLICY</c>, a decision).</summary>
public interface IRoutePolicy
{
    /// <summary>Indexes into <c>request.Specs</c> to grant, strictly ascending.</summary>
    ValueTask<IReadOnlyList<uint>> GrantAsync(ExposeRoutePolicyRequest request, CancellationToken cancellationToken);
}

/// <summary>
/// One upcall table per runtime (C ABI 4.1, design <c>docs/_internal/bindings/upcall-design.md</c> §4):
/// Rust asks .NET a question as an event, .NET answers by invocation id.
///
/// <para>A dedicated poll thread drains <c>aster_upcall_poll</c> (the blocking native wait never
/// occupies a thread-pool thread) and runs each upcall's handler as its own task, so a slow
/// handler holds nothing else up and answers may come in any order. A handler exception answers
/// <c>ERROR</c> with its message; a kind without a handler answers <c>UNIMPLEMENTED</c>. When Rust
/// stops waiting (timeout, abandoned caller, shutdown) the handler's
/// <see cref="UpcallContext.Cancellation"/> fires.</para>
///
/// <para>Disposal: the table is closed first (pending upcalls fail, decisions take their failure
/// mode), the poll thread is joined, running handlers are cancelled and awaited (bounded), then the
/// native handle is freed. No handler starts after disposal begins. Adapters built on this table
/// (<see cref="NativeLeaseSerializer"/>, <see cref="NativeFencedResource"/>, <see cref="AdmittedFilterHandle"/>,
/// expose registrations) can no longer reach .NET afterwards; dispose them first or let them fail.</para>
/// </summary>
public sealed class Upcalls : IAsyncDisposable, IDisposable
{
    /// <summary><c>ASTER_UPCALL_DEFAULT_CAPACITY</c>.</summary>
    public const int DefaultCapacity = 1024;

    internal const int EventSize = 40;
    private const int Batch = 32;
    private const uint PollTimeoutMs = 100;
    private static readonly TimeSpan HandlerDrainTimeout = TimeSpan.FromSeconds(5);

    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private readonly ConcurrentDictionary<ushort, UpcallHandler> _handlers = new();
    private readonly ConcurrentDictionary<ulong, UpcallContext> _inFlight = new();
    private readonly TaskCompletionSource _drained = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly IntPtr _events;
    private readonly Thread _pollThread;
    private int _active;
    private int _closing;
    private volatile bool _freed;
    private long _cancels;
    private int _disposeStarted;
    private Task? _disposeTask;

    private unsafe Upcalls(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
        _events = (IntPtr)System.Runtime.InteropServices.NativeMemory.AllocZeroed((nuint)(EventSize * Batch));
        _pollThread = new Thread(PollLoop) { IsBackground = true, Name = $"aster-upcalls-{handle}" };
        _pollThread.Start();
    }

    /// <summary>A table queueing at most <paramref name="capacity"/> upcalls (0 = default 1024); beyond it upcalls are refused at once (decisions take their failure mode, other kinds fail).</summary>
    public static Upcalls Create(Runtime runtime, int capacity = 0)
    {
        ArgumentNullException.ThrowIfNull(runtime);
        runtime.Retain();
        try
        {
            Native.Check(Native.aster_upcalls_new(runtime.Handle, (uint)capacity, out ulong h), "aster_upcalls_new");
            return new Upcalls(runtime, h);
        }
        catch
        {
            runtime.Release();
            throw;
        }
    }

    public Runtime Runtime => _runtime;

    /// <summary>The native <c>aster_upcalls_t</c>; valid until disposal.</summary>
    internal ulong NativeHandle => _handle;

    /// <summary>Cancel events received so far (diagnostics / tests).</summary>
    public long CancelCount => Interlocked.Read(ref _cancels);

    /// <summary>Handlers currently running (diagnostics / tests).</summary>
    public int ActiveHandlers => Volatile.Read(ref _active);

    // ── policy ───────────────────────────────────────────────────────────────

    /// <summary>Timeout and failure mode of one kind (default 5 s, fail closed). The failure mode applies to decision kinds.</summary>
    public Upcalls SetPolicy(UpcallKind kind, TimeSpan timeout, HookFailureMode mode)
    {
        ThrowIfDisposed();
        Native.Check(Native.aster_upcall_set_policy(_runtime.Handle, _handle, (uint)kind, Millis(timeout), (uint)mode), "aster_upcall_set_policy");
        return this;
    }

    /// <summary>The policy of every kind without its own.</summary>
    public Upcalls SetDefaultPolicy(TimeSpan timeout, HookFailureMode mode)
    {
        ThrowIfDisposed();
        Native.Check(Native.aster_upcall_set_default_policy(_runtime.Handle, _handle, Millis(timeout), (uint)mode), "aster_upcall_set_default_policy");
        return this;
    }

    private static uint Millis(TimeSpan d) => (uint)Math.Max(1, Math.Min(int.MaxValue, (long)d.TotalMilliseconds));

    // ── handlers ─────────────────────────────────────────────────────────────

    /// <summary>Registers a raw handler for <paramref name="kind"/> (replaces any earlier one).</summary>
    public Upcalls Handle(UpcallKind kind, UpcallHandler handler)
    {
        if (kind == UpcallKind.Cancel) throw new ArgumentException("CANCEL is not answerable", nameof(kind));
        ArgumentNullException.ThrowIfNull(handler);
        _handlers[(ushort)kind] = handler;
        return this;
    }

    /// <summary>Serves <c>LEASE_TRANSITION</c> and <c>LEASE_SNAPSHOT</c> with <paramref name="serializer"/>.</summary>
    public Upcalls ServeLeaseSerializer(ILeaseSerializer serializer)
    {
        Handle(UpcallKind.LeaseTransition, async (req, ctx) =>
        {
            var r = Ucv1.DecodeLeaseTransitionRequest(req);
            return Ucv1.EncodeTransitionOutcome(await serializer.TransitionAsync(r.Resource, r.Op, ctx.Cancellation).ConfigureAwait(false));
        });
        return Handle(UpcallKind.LeaseSnapshot, async (req, ctx) =>
            Ucv1.EncodeLeaseSnapshot(await serializer.SnapshotAsync(Ucv1.DecodeLeaseSnapshotRequest(req), ctx.Cancellation).ConfigureAwait(false)));
    }

    /// <summary>Serves <c>LEASE_AUTHORIZE</c> with <paramref name="authorizer"/>.</summary>
    public Upcalls ServeLeaseAuthorizer(ILeaseAuthorizer authorizer)
        => Handle(UpcallKind.LeaseAuthorize, async (req, ctx) =>
            Ucv1.EncodeDecision(await authorizer.AuthorizeAsync(Ucv1.DecodeLeaseAuthorizeRequest(req), ctx.Cancellation).ConfigureAwait(false)));

    /// <summary>Serves <c>FENCED_MUTATE</c> with <paramref name="resource"/>.</summary>
    public Upcalls ServeFencedResource(IFencedResource resource)
        => Handle(UpcallKind.FencedMutate, async (req, ctx) =>
        {
            var m = Ucv1.DecodeFencedMutateRequest(req);
            var res = await resource.MutateAsync(m.Fence, m.Writer, m.Op, ctx.Cancellation).ConfigureAwait(false);
            return Ucv1.EncodeFencedMutateResponse(res ?? throw new InvalidOperationException("IFencedResource.MutateAsync returned null"));
        });

    /// <summary>Serves <c>TOPOLOGY_ADMITTED</c> with <paramref name="filter"/>.</summary>
    public Upcalls ServeAdmittedFilter(IAdmittedFilter filter)
        => Handle(UpcallKind.TopologyAdmitted, async (req, ctx) =>
        {
            var ids = Ucv1.DecodeTopologyAdmittedRequest(req);
            var answers = await filter.AdmittedAsync(ids, ctx.Cancellation).ConfigureAwait(false);
            if (answers == null || answers.Count != ids.Count)
                throw new InvalidOperationException($"IAdmittedFilter must answer once per id ({ids.Count})");
            return Ucv1.EncodeTopologyAdmittedResponse(answers);
        });

    /// <summary>Serves <c>EXPOSE_AUTHORIZE</c> (admission of a peer to an exposed service) with <paramref name="authorizer"/>.</summary>
    public Upcalls ServeExposeAuthorizer(IExposeAuthorizer authorizer)
        => Handle(UpcallKind.ExposeAuthorize, async (req, ctx) =>
            Ucv1.EncodeDecision(await authorizer.AuthorizeAsync(Ucv1.DecodeExposeAuthorizeRequest(req), ctx.Cancellation).ConfigureAwait(false)));

    /// <summary>Serves <c>EXPOSE_ROUTE_POLICY</c> with <paramref name="policy"/>.</summary>
    public Upcalls ServeRoutePolicy(IRoutePolicy policy)
        => Handle(UpcallKind.ExposeRoutePolicy, async (req, ctx) =>
            Ucv1.EncodeExposeRouteGrant(await policy.GrantAsync(Ucv1.DecodeExposeRoutePolicyRequest(req), ctx.Cancellation).ConfigureAwait(false)));

    // ── poll loop ────────────────────────────────────────────────────────────

    private unsafe void PollLoop()
    {
        while (Volatile.Read(ref _closing) == 0)
        {
            uint n;
            try
            {
                // Blocks up to PollTimeoutMs; wakes at once when the table is closed.
                n = Native.aster_upcall_poll(_runtime.Handle, _handle, (UpcallEventNative*)_events, Batch, PollTimeoutMs);
            }
            catch (Exception ex)
            {
                if (Volatile.Read(ref _closing) != 0) return;
                Console.Error.WriteLine($"aster upcall poll error: {ex.Message}");
                Thread.Sleep(10);
                continue;
            }

            for (uint i = 0; i < n; i++)
            {
                UpcallEventNative ev = ((UpcallEventNative*)_events)[i];
                byte[] payload = TakePayload(in ev);
                if (Volatile.Read(ref _closing) != 0) continue; // closing: buffer released, nothing starts
                try { Dispatch(in ev, payload); }
                catch (Exception ex) { Console.Error.WriteLine($"aster upcall dispatch failed: {ex.Message}"); }
            }
        }
    }

    /// <summary>Copies the payload and releases its native buffer exactly once.</summary>
    private unsafe byte[] TakePayload(in UpcallEventNative ev)
    {
        byte[] payload = ev.payload_len == 0 || ev.payload_ptr == IntPtr.Zero
            ? Array.Empty<byte>()
            : new ReadOnlySpan<byte>((void*)ev.payload_ptr, (int)ev.payload_len).ToArray();
        if (ev.buffer != 0)
            Native.aster_upcall_buffer_release(_runtime.Handle, _handle, ev.buffer);
        return payload;
    }

    private void Dispatch(in UpcallEventNative ev, byte[] payload)
    {
        ulong id = ev.invocation_id;
        if (ev.kind == (ushort)UpcallKind.Cancel)
        {
            Interlocked.Increment(ref _cancels);
            if (_inFlight.TryGetValue(id, out var running))
                running.Cancel((UpcallCancelReason)ev.cancel_reason);
            return;
        }

        if (!_handlers.TryGetValue(ev.kind, out var handler))
        {
            Respond(id, UpcallStatus.Unimplemented, Array.Empty<byte>());
            return;
        }

        var ctx = new UpcallContext(this, id, (UpcallKind)ev.kind, (long)ev.deadline_unix_ms);
        _inFlight[id] = ctx;
        Interlocked.Increment(ref _active);
        // User code never runs on the poll thread.
        _ = Task.Run(() => RunAsync(handler, ctx, payload));
    }

    private async Task RunAsync(UpcallHandler handler, UpcallContext ctx, byte[] payload)
    {
        try
        {
            if (ctx.Cancellation.IsCancellationRequested || Volatile.Read(ref _closing) != 0) return;
            byte[]? result = await handler(payload, ctx).ConfigureAwait(false);
            if (ctx.TryMarkAnswered() && !ctx.Cancellation.IsCancellationRequested)
                Respond(ctx.InvocationId, UpcallStatus.Ok, result ?? Array.Empty<byte>());
        }
        catch (OperationCanceledException) when (ctx.Cancellation.IsCancellationRequested)
        {
            // Rust stopped waiting (or we are closing): nobody to answer.
        }
        catch (Exception ex)
        {
            if (ctx.TryMarkAnswered())
                Respond(ctx.InvocationId, UpcallStatus.Error, Encoding.UTF8.GetBytes(string.IsNullOrEmpty(ex.Message) ? ex.GetType().Name : ex.Message));
        }
        finally
        {
            _inFlight.TryRemove(new KeyValuePair<ulong, UpcallContext>(ctx.InvocationId, ctx));
            ctx.Dispose();
            if (Interlocked.Decrement(ref _active) == 0 && Volatile.Read(ref _closing) != 0)
                _drained.TrySetResult();
        }
    }

    /// <summary>
    /// Answers <paramref name="invocationId"/>. Returns false when Rust no longer waits for it
    /// (answered, cancelled, timed out) -- the normal outcome of a late answer.
    /// </summary>
    public unsafe bool Respond(ulong invocationId, UpcallStatus status, ReadOnlySpan<byte> payload)
    {
        if (_freed || Volatile.Read(ref _closing) != 0) return false;
        int rc;
        fixed (byte* p = payload)
            rc = Native.aster_upcall_respond(_runtime.Handle, _handle, invocationId, (uint)status, payload.IsEmpty ? null : p, (uint)payload.Length);
        if (rc == 0) return true;
        if (rc == (int)Status.NotFound) return false;
        throw IrohException.FromStatus(rc, "aster_upcall_respond");
    }

    // ── disposal ─────────────────────────────────────────────────────────────

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(Volatile.Read(ref _closing) != 0, this);

    /// <summary>
    /// Closes the table only: pending upcalls fail now (decisions take their failure mode). The poll
    /// loop and handlers keep running; call <see cref="DisposeAsync"/> to finish.
    /// </summary>
    public void CloseTable()
    {
        ThrowIfDisposed();
        Native.Check(Native.aster_upcalls_close(_runtime.Handle, _handle), "aster_upcalls_close");
    }

    /// <summary>See the class remarks for the ordering.</summary>
    public ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposeStarted, 1) == 0)
            _disposeTask = Task.Run(DisposeCore);
        return new ValueTask(_disposeTask!);
    }

    public void Dispose() => DisposeAsync().AsTask().GetAwaiter().GetResult();

    private unsafe void FreeEvents() => System.Runtime.InteropServices.NativeMemory.Free((void*)_events);

    private async Task DisposeCore()
    {
        // Wake the poll thread and fail pending Rust waiters before anything else.
        Interlocked.Exchange(ref _closing, 1);
        try { Native.aster_upcalls_close(_runtime.Handle, _handle); } catch { /* runtime already closed */ }

        bool pollStopped = _pollThread.Join(TimeSpan.FromSeconds(5));

        foreach (var running in _inFlight.Values) running.Cancel(UpcallCancelReason.Shutdown);
        if (Volatile.Read(ref _active) == 0) _drained.TrySetResult();
        await Task.WhenAny(_drained.Task, Task.Delay(HandlerDrainTimeout)).ConfigureAwait(false);

        _freed = true;
        try { Native.aster_upcalls_free(_runtime.Handle, _handle); } catch { /* runtime already closed */ }
        // A poll thread that did not stop still owns the event array: leak it rather than free it under the thread.
        if (pollStopped) FreeEvents();
        _runtime.Release();
    }
}
