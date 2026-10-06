using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// Owns a native Aster runtime and drives the completion queue poll loop.
///
/// The poll loop runs on a dedicated background thread and dispatches events
/// to registered TaskCompletionSources. This matches the same architecture
/// as Go's runtime poller and Java's IrohPollThread.
/// </summary>
public sealed class Runtime : IDisposable
{
    private readonly ulong _handle;
    private readonly Thread _pollThread;
    private readonly CancellationTokenSource _cts = new();
    // Replaced on every successful poll. A submitter snapshots this before its
    // native attempt, so a poll racing with QUEUE_FULL cannot be missed and old
    // progress cannot accumulate into a retry spin.
    private readonly object _queueProgressLock = new();
    private TaskCompletionSource<bool> _queueProgress = NewProgressTcs();
    // Only one submitter retries after saturation. This prevents a poll from
    // waking an unbounded retry herd and lets every waiter make progress.
    private readonly SemaphoreSlim _submissionTurn = new(1, 1);

    // Maps operation ID -> TaskCompletionSource that receives the event.
    private readonly ConcurrentDictionary<ulong, TaskCompletionSource<Event>> _ops = new();

    // Unsolicited events (operation == 0, e.g. admission hooks) routed by event.handle.
    private readonly ConcurrentDictionary<ulong, Action<Event>> _inbound = new();

    // Guards dispose so the poll loop exits cleanly.
    private volatile bool _disposed;

    // Owners sharing this runtime (the creator + nodes started on a gate's runtime).
    private int _refCount = 1;
    private int _releasedByDispose;

    private const int MaxEvents = 64;

    /// <summary>
    /// Creates a new Aster runtime and starts the poll loop.
    /// </summary>
    public Runtime()
        : this(RuntimeConfig.Default)
    {
    }

    /// <summary>
    /// Creates a new Aster runtime with custom config and starts the poll loop.
    /// </summary>
    public Runtime(RuntimeConfig config)
    {
        uint loaded = Native.aster_abi_version();
        if (loaded >> 16 != Native.AbiVersionMajor)
            throw new IrohException(Status.AbiMismatch,
                $"native library ABI {loaded >> 16}.{(loaded >> 8) & 0xff}.{loaded & 0xff} " +
                $"does not match binding ABI major {Native.AbiVersionMajor}");
        if (((loaded >> 8) & 0xff) < Native.RequiredAbiMinor)
            throw new IrohException(Status.AbiMismatch,
                $"native library ABI {loaded >> 16}.{(loaded >> 8) & 0xff} is too old: this binding needs " +
                $"{Native.AbiVersionMajor}.{Native.RequiredAbiMinor}+ (rebuild libaster_transport_ffi or set -p:NativeLibraryPath)");

        config.struct_size = (uint)Marshal.SizeOf<RuntimeConfig>();
        int r = Native.aster_runtime_new(Native.AbiVersionMajor, in config, out _handle);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_runtime_new");

        _pollThread = new Thread(PollLoop)
        {
            Name = "iroh-poll",
            IsBackground = true,
        };
        _pollThread.Start();
    }

    /// <summary>
    /// The native runtime handle.
    /// </summary>
    public ulong Handle => _handle;

    /// <summary>
    /// Returns the ABI version of the native library.
    /// </summary>
    public static (int Major, int Minor, int Patch) Version
    {
        get
        {
            // Cheap, stateless query — no handle needed.
            uint v = Native.aster_abi_version();
            return ((int)(v >> 16), (int)((v >> 8) & 0xff), (int)(v & 0xff));
        }
    }

    /// <summary>
    /// Registers an operation ID and returns a Task that completes when the event arrives.
    /// The returned Task is guaranteed to have completed by the time the event is dispatched.
    /// </summary>
    public Task<Event> WaitForAsync(ulong opId, CancellationToken cancellationToken = default)
    {
        if (_disposed)
            throw new ObjectDisposedException(nameof(Runtime));

        // The native submit has already returned opId, so the completion may
        // have been dispatched before we get here. Both sides use GetOrAdd so
        // whichever arrives first creates the TCS and the other finds it.
        var tcs = _ops.GetOrAdd(opId, NewTcs);

        if (tcs.Task.IsCompleted)
        {
            _ops.TryRemove(new KeyValuePair<ulong, TaskCompletionSource<Event>>(opId, tcs));
            return tcs.Task;
        }

        CancellationTokenRegistration registration = default;
        if (cancellationToken.CanBeCanceled)
        {
            registration = cancellationToken.Register(
                state => ((TaskCompletionSource<Event>)state!).TrySetCanceled(),
                tcs, false);
        }

        // When the Task completes (success, cancel, or fault), unregister.
        tcs.Task.ContinueWith(
            (completedTask, state) =>
            {
                var (ops, id, t, reg) = ((ConcurrentDictionary<ulong, TaskCompletionSource<Event>>, ulong, TaskCompletionSource<Event>, CancellationTokenRegistration))state!;
                ops.TryRemove(new KeyValuePair<ulong, TaskCompletionSource<Event>>(id, t));
                reg.Dispose();
            },
            (_ops, opId, tcs, registration),
            CancellationToken.None,
            TaskContinuationOptions.ExecuteSynchronously,
            TaskScheduler.Default);

        return tcs.Task;
    }

    /// <summary>
    /// Starts an asynchronous native operation, waiting for completion-queue space when the
    /// native runtime reports <see cref="Status.QueueFull"/>. The submit callback must keep
    /// native argument storage alive only for the duration of one invocation; it may be called
    /// more than once, but a retry happens only after the native pre-start refusal.
    /// </summary>
    internal async Task<ulong> SubmitAsync(
        NativeOperation submit,
        string what,
        CancellationToken cancellationToken = default)
    {
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, _cts.Token);
        try
        {
            await _submissionTurn.WaitAsync(linked.Token).ConfigureAwait(false);
            try
            {
                while (true)
                {
                    ObjectDisposedException.ThrowIf(_disposed, this);
                    Task progress = QueueProgressTask();
                    int status = submit(out ulong operation);
                    if (status == 0)
                        return operation;
                    if (status != (int)Status.QueueFull)
                        throw IrohException.FromStatus(status, what);
                    await progress.WaitAsync(linked.Token).ConfigureAwait(false);
                }
            }
            finally
            {
                _submissionTurn.Release();
            }
        }
        catch (OperationCanceledException) when (_cts.IsCancellationRequested && !cancellationToken.IsCancellationRequested)
        {
            throw new ObjectDisposedException(nameof(Runtime));
        }
    }

    /// <summary>
    /// Convenience: waits for an operation and returns the event handle.
    /// </summary>
    public async Task<Event> WaitForAsync(
        ulong opId,
        TimeSpan timeout,
        CancellationToken cancellationToken = default)
    {
        var delayTask = Task.Delay(timeout, cancellationToken);
        var eventTask = WaitForAsync(opId, cancellationToken);

        var completed = await Task.WhenAny(eventTask, delayTask).ConfigureAwait(false);
        if (completed == delayTask)
            throw new OperationCanceledException("Operation timed out");

        return await eventTask.ConfigureAwait(false);
    }

    /// <summary>Adds an owner; the runtime closes when every owner has released it.</summary>
    internal Runtime Retain()
    {
        if (Interlocked.Increment(ref _refCount) <= 1)
            throw new ObjectDisposedException(nameof(Runtime));
        return this;
    }

    /// <summary>Drops one owner (see <see cref="Retain"/>).</summary>
    internal void Release()
    {
        if (Interlocked.Decrement(ref _refCount) == 0)
            Close();
    }

    /// <summary>
    /// Routes unsolicited events (operation 0) whose <c>handle</c> is <paramref name="handle"/>
    /// to <paramref name="handler"/>, called on the poll thread. The event's data buffer is
    /// released after the handler returns, so copy what you need.
    /// </summary>
    internal void RegisterInbound(ulong handle, Action<Event> handler) => _inbound[handle] = handler;

    internal void UnregisterInbound(ulong handle) => _inbound.TryRemove(handle, out _);

    /// <summary>
    /// Releases this owner's reference; the native runtime and the poll loop stop once nodes
    /// and gates sharing the runtime have been disposed too.
    /// </summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _releasedByDispose, 1) == 0)
            Release();
    }

    private void Close()
    {
        if (_disposed)
            return;
        _disposed = true;

        _cts.Cancel();

        // Wake the poll thread so it exits.
        // Signal with a zero-timeout poll to break out of the sleep.
        _pollThread.Join(TimeSpan.FromSeconds(5));

        Native.aster_runtime_close(_handle);
        _cts.Dispose();
    }

    private void PollLoop()
    {
        var events = new Event[MaxEvents];
        var timeoutMs = 10; // 10ms per poll

        while (!_disposed)
        {
            int n;
            try
            {
                // Marshal events directly into the managed array.
                // LibraryImport doesn't support passing managed arrays directly,
                // so we pin the array and pass the pinned pointer.
                n = PollEvents(events, timeoutMs);
            }
            catch (Exception ex) when (!(ex is OperationCanceledException))
            {
                Console.Error.WriteLine($"iroh poll error: {ex.Message}");
                Thread.Sleep(timeoutMs);
                continue;
            }

            if (n > 0)
            {
                DispatchBatch(events, n);
                SignalQueueProgress();
            }

            // If we got fewer than MaxEvents, the queue may be drained;
            // loop immediately without sleeping to maintain low latency.
        }

        // Drain remaining events before exiting.
        DrainEvents();
    }

    private int PollEvents(Event[] events, int timeoutMs)
    {
        // Pin the array so we can pass its address to native code.
        GCHandle handle = GCHandle.Alloc(events, GCHandleType.Pinned);
        try
        {
            IntPtr ptr = handle.AddrOfPinnedObject();
            return (int)(nuint)Native.aster_poll_events(_handle, ptr, (UIntPtr)Math.Min(MaxEvents, events.Length), (uint)timeoutMs);
        }
        finally
        {
            handle.Free();
        }
    }

    private void DispatchBatch(Event[] events, int count)
    {
        for (int i = 0; i < count; i++)
        {
            ref readonly Event ev = ref events[i];

            if (ev.operation == 0)
            {
                if (_inbound.TryGetValue(ev.handle, out var handler))
                {
                    try { handler(ev); }
                    catch (Exception ex) { Console.Error.WriteLine($"aster inbound handler failed: {ex.Message}"); }
                }
                if (ev.buffer != 0)
                    Native.aster_buffer_release(_handle, ev.buffer);
                continue;
            }
            // If no waiter is registered yet, park the completed TCS for it.
            // (A registered waiter's continuation removes the entry.)
            _ops.GetOrAdd(ev.operation, NewTcs).TrySetResult(ev);
        }
    }

    private static TaskCompletionSource<Event> NewTcs(ulong _)
        => new(TaskCreationOptions.RunContinuationsAsynchronously);

    private static TaskCompletionSource<bool> NewProgressTcs()
        => new(TaskCreationOptions.RunContinuationsAsynchronously);

    private Task QueueProgressTask()
    {
        lock (_queueProgressLock)
            return _queueProgress.Task;
    }

    private void SignalQueueProgress()
    {
        TaskCompletionSource<bool> previous;
        lock (_queueProgressLock)
        {
            previous = _queueProgress;
            _queueProgress = NewProgressTcs();
        }
        previous.TrySetResult(true);
    }

    private void DrainEvents()
    {
        var events = new Event[MaxEvents];
        while (true)
        {
            int n = PollEvents(events, 0);
            if (n == 0)
                break;
            DispatchBatch(events, n);
            SignalQueueProgress();
        }
    }

    /// <summary>
    /// Releases a buffer that was allocated by Rust and passed to an event.
    /// </summary>
    public void ReleaseBuffer(ulong buffer)
    {
        int r = Native.aster_buffer_release(_handle, buffer);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_buffer_release");
    }

    /// <summary>
    /// Cancels an in-flight operation.
    /// </summary>
    public void Cancel(ulong opId)
    {
        if (_ops.TryRemove(opId, out var pending))
            pending.TrySetCanceled();
        int r = Native.aster_operation_cancel(_handle, opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "aster_operation_cancel");
    }
}

internal delegate int NativeOperation(out ulong operation);
