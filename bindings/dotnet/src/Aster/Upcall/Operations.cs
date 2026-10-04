using System;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Upcall;

/// <summary>Completion plumbing shared by the 4.1 / 4.2 operations (lease, fenced, topology, expose).</summary>
internal static class Operations
{
    /// <summary>
    /// Awaits operation <paramref name="op"/>: a <c>ASTER_EVENT_ERROR</c> becomes an
    /// <see cref="IrohException"/>, any kind other than <paramref name="expected"/> is a protocol
    /// error, and the payload is copied and its buffer released exactly once. Cancelling
    /// <paramref name="ct"/> abandons the wait and suppresses the completion event
    /// (<c>aster_operation_cancel</c>); it does not stop work Rust already started, so an upcall
    /// behind the operation lives until its kind's timeout.
    /// </summary>
    public static async Task<(Event Ev, byte[] Data)> AwaitAsync(Runtime runtime, ulong op, EventKind expected, string what, CancellationToken ct)
    {
        Event ev;
        try
        {
            ev = await runtime.WaitForAsync(op, ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            try { runtime.Cancel(op); } catch (IrohException) { /* already completed */ }
            throw;
        }

        if (ev.kind == (uint)EventKind.Error || ev.status != 0 && ev.kind != (uint)expected)
            throw EventError.From(runtime, ev, what);
        if (ev.kind != (uint)expected)
        {
            byte[] stray = EventError.TakePayload(runtime, in ev);
            throw new IrohException($"{what}: unexpected event {(EventKind)ev.kind} ({stray.Length} bytes)");
        }
        return (ev, EventError.TakePayload(runtime, in ev));
    }
}
