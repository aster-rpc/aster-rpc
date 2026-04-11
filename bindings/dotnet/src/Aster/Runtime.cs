using System;
using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// Owns a native iroh runtime and drives the completion queue poll loop.
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

    // Maps operation ID -> TaskCompletionSource that receives the event.
    private readonly ConcurrentDictionary<ulong, TaskCompletionSource<Event>> _ops = new();

    // Guards dispose so the poll loop exits cleanly.
    private volatile bool _disposed;

    // Batch buffer allocated once and reused across polls.
    private readonly IntPtr _eventBuffer;
    private const int MaxEvents = 64;

    /// <summary>
    /// Creates a new iroh runtime and starts the poll loop.
    /// </summary>
    public Runtime()
        : this(RuntimeConfig.Default)
    {
    }

    /// <summary>
    /// Creates a new iroh runtime with custom config and starts the poll loop.
    /// </summary>
    public Runtime(RuntimeConfig config)
    {
        int r = Native.iroh_runtime_new(ref config, out _handle);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_runtime_new");

        // Pre-allocate the event batch buffer.
        // Each Event is ~64 bytes; 64 * 64 = 4KB, negligible.
        _eventBuffer = Marshal.AllocHGlobal(Marshal.SizeOf<Event>() * MaxEvents);

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
            // These are cheap, stateless queries — no handle needed.
            return (Native.iroh_abi_version_major(), Native.iroh_abi_version_minor(), Native.iroh_abi_version_patch());
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

        var tcs = new TaskCompletionSource<Event>(TaskCreationOptions.RunContinuationsAsynchronously);

        // If the caller cancels, remove the registration so the event is dropped.
        using var registration = cancellationToken.Register(
            state => ((TaskCompletionSource<Event>)state!).TrySetCanceled(),
            tcs, false);

        // Store before returning to avoid a race where the event arrives between
        // Register() completing and storing in the map.
        _ops[opId] = tcs;

        // When the Task completes (success, cancel, or fault), unregister.
        tcs.Task.ContinueWith(
            (completedTask, state) =>
            {
                var (ops, id) = ((ConcurrentDictionary<ulong, TaskCompletionSource<Event>>, ulong))state!;
                ops.TryRemove(id, out _);
            },
            (_ops, opId),
            CancellationToken.None,
            TaskContinuationOptions.ExecuteSynchronously,
            TaskScheduler.Default);

        return tcs.Task;
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

    /// <summary>
    /// Releases the native runtime and stops the poll loop.
    /// </summary>
    public void Dispose()
    {
        if (_disposed)
            return;
        _disposed = true;

        _cts.Cancel();

        // Wake the poll thread so it exits.
        // Signal with a zero-timeout poll to break out of the sleep.
        _pollThread.Join(TimeSpan.FromSeconds(5));

        Native.iroh_runtime_close(_handle);

        Marshal.FreeHGlobal(_eventBuffer);
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
            return Native.iroh_poll_events(_handle, ptr, MaxEvents, timeoutMs);
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

            if (_ops.TryRemove(ev.operation, out var tcs))
            {
                tcs.TrySetResult(ev);
            }
        }
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
        }
    }

    /// <summary>
    /// Releases a buffer that was allocated by Rust and passed to an event.
    /// </summary>
    public void ReleaseBuffer(ulong buffer)
    {
        int r = Native.iroh_buffer_release(_handle, buffer);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_buffer_release");
    }

    /// <summary>
    /// Cancels an in-flight operation.
    /// </summary>
    public void Cancel(ulong opId)
    {
        _ops.TryRemove(opId, out _);
        int r = Native.iroh_operation_cancel(_handle, opId);
        if (r != 0)
            throw IrohException.FromStatus(r, "iroh_operation_cancel");
    }
}
