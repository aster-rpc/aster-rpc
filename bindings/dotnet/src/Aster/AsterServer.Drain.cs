using System;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// What <see cref="AsterServer.DrainAsync"/> did (<c>aster_drain_report_t</c>, ABI 7.2). Calls count
/// those admitted to a handler, not those refused at routing.
/// </summary>
/// <param name="CallsInFlight">Calls running when the drain started.</param>
/// <param name="CallsCompleted">Calls that finished before the deadline (those in flight at the start, and calls on existing sessions admitted during the drain).</param>
/// <param name="CallsCancelled">Calls still running at the deadline, cancelled by closing their connection.</param>
/// <param name="CallsRefused">New calls answered <c>UNAVAILABLE</c> with the <c>aster.draining</c> detail.</param>
/// <param name="ConnectionsClosed">Connections the drain closed with <see cref="AsterServer.DrainCloseCode"/>.</param>
/// <param name="ConnectionsRefused">New connections refused at accept.</param>
/// <param name="DeadlineReached">The grace ended with work outstanding.</param>
/// <param name="Elapsed">How long the drain took.</param>
public sealed record DrainReport(
    ulong CallsInFlight, ulong CallsCompleted, ulong CallsCancelled, ulong CallsRefused,
    ulong ConnectionsClosed, ulong ConnectionsRefused, bool DeadlineReached, TimeSpan Elapsed);

public sealed partial class AsterServer
{
    /// <summary>
    /// The QUIC application close code a draining server closes connections with: "this server is going
    /// away; reconnect to another replica" (<c>ASTER_DRAIN_CLOSE_CODE</c>).
    /// </summary>
    public const ulong DrainCloseCode = Native.DrainCloseCode;

    private int _draining;

    /// <summary>Whether <see cref="DrainAsync"/> has started.</summary>
    public bool IsDraining => Volatile.Read(ref _draining) != 0;

    /// <summary>
    /// Takes the server out of rotation, then disposes it (Rust <c>AsterServer::drain</c>):
    /// <list type="number">
    /// <item>an attached health listener reports not ready;</item>
    /// <item>new connections are closed right after their handshake with <see cref="DrainCloseCode"/>, and
    /// new calls answer <c>UNAVAILABLE</c> with the <c>aster.draining</c> detail
    /// (<see cref="Interceptors.RpcError.IsDraining"/>); calls on a session that already exists still run;</item>
    /// <item>each connection is closed with <see cref="DrainCloseCode"/> once it has no call in flight and no live session;</item>
    /// <item>in-flight calls (every pattern) run until <paramref name="grace"/>; then every remaining connection
    /// is closed, which cancels the handlers' <see cref="Rpc.ServerCallContext.CancellationToken"/>, and the
    /// drain waits up to 2 s for them to end.</item>
    /// </list>
    /// Cancelling <paramref name="cancellationToken"/> cuts the grace short: the remaining calls are cancelled
    /// as at the grace deadline, and the report is still returned. A later call returns the same report.
    /// </summary>
    public async Task<DrainReport> DrainAsync(TimeSpan grace, CancellationToken cancellationToken = default)
    {
        if (grace < TimeSpan.Zero) throw new ArgumentOutOfRangeException(nameof(grace), "the drain grace must not be negative");
        ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
        Interlocked.Exchange(ref _draining, 1);
        uint graceMs = DrainMillis(grace);
        // aster_server_drain blocks (and refuses a tokio thread); the poll thread keeps lowering
        // the calls it waits for, so run it on the pool.
        var drain = Task.Run(() => _dispatch.Drain(graceMs), CancellationToken.None);
        DrainReport report;
        using (cancellationToken.Register(static s =>
               {
                   // A second drain with no grace moves the deadline to now; it returns once the first has finished.
                   var d = (DispatchServer)s!;
                   _ = Task.Run(() => { try { d.Drain(0); } catch (Exception) { /* the first drain reports */ } });
               }, _dispatch))
        {
            report = await drain.ConfigureAwait(false);
        }
        await DisposeAsync().ConfigureAwait(false);
        return report;
    }

    internal static uint DrainMillis(TimeSpan d)
    {
        // Round up so a sub-millisecond grace is not "now".
        ulong ms = (ulong)(d.Ticks / TimeSpan.TicksPerMillisecond);
        if (d.Ticks % TimeSpan.TicksPerMillisecond != 0) ms++;
        return (uint)Math.Min(ms, uint.MaxValue);
    }
}
