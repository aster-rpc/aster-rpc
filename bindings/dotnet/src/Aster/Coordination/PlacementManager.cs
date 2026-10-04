using System;
using System.Threading;
using System.Threading.Tasks;
using Aster.Upcall;

namespace Aster.Coordination;

/// <summary>The placement policy provider (upcall kind 32).</summary>
public interface IPlacementPolicySource
{
    ValueTask<PlacementPolicy> GetPolicyAsync(CancellationToken cancellationToken);
}

/// <summary>
/// Brings a node up as a member (upcall kind 33). The request is idempotent and may repeat. Answer with a
/// <see cref="MemberSeed"/> <b>for the node asked about</b>; a seed for another node fails the pass.
/// </summary>
public interface IPlacementProvisioner
{
    ValueTask<MemberSeed> ProvisionAsync(ProvisionRequest request, CancellationToken cancellationToken);
}

/// <summary>A serialized shared topology view (upcall kind 17), used when the manager is not given a node to read it from.</summary>
public interface ITopologySource
{
    ValueTask<TopologySnapshot> GetTopologyAsync(CancellationToken cancellationToken);
}

/// <summary>Registers the placement providers on an <see cref="Upcalls"/> table.</summary>
public static class PlacementUpcalls
{
    /// <summary>
    /// Serves kinds 32 (policy) and 33 (provisioner), and 17 (topology) when <paramref name="topology"/> is given. These are not
    /// decision kinds: an error, <c>UNIMPLEMENTED</c>, timeout, overload, closed table or undecodable answer fails the reconcile pass; there
    /// is no fail-open. Set the per-kind timeouts with <see cref="Upcalls.SetPolicy"/>.
    /// </summary>
    public static Upcalls ServePlacement(this Upcalls upcalls, IPlacementPolicySource policy, IPlacementProvisioner provisioner, ITopologySource? topology = null)
    {
        ArgumentNullException.ThrowIfNull(policy);
        ArgumentNullException.ThrowIfNull(provisioner);
        upcalls.Handle(UpcallKind.PlacementPolicy, async (req, ctx) =>
        {
            CoordinationCodec.DecodePlacementPolicyRequest(req);
            return CoordinationCodec.EncodePlacementPolicy(await policy.GetPolicyAsync(ctx.Cancellation).ConfigureAwait(false));
        });
        upcalls.Handle(UpcallKind.PlacementProvision, async (req, ctx) =>
        {
            var r = CoordinationCodec.DecodeProvisionRequest(req);
            return CoordinationCodec.EncodeMemberSeed(await provisioner.ProvisionAsync(r, ctx.Cancellation).ConfigureAwait(false));
        });
        if (topology != null)
        {
            upcalls.Handle(UpcallKind.TopologySnapshot, async (req, ctx) =>
            {
                Ucv1.DecodeTopologySnapshotRequest(req);
                return Ucv1.EncodeTopologySnapshot(await topology.GetTopologyAsync(ctx.Cancellation).ConfigureAwait(false));
            });
        }
        return upcalls;
    }
}

/// <summary>
/// A placement manager over a <see cref="CoordinationClient"/> (<c>aster_coordination_placement_manager_t</c>): each
/// <see cref="ReconcileAsync"/> pass reads the policy (kind 32) and the topology (the given node's shared view, else kind 17), asks the
/// provisioner (kind 33) for members it must bring up, and changes membership only through the client's durable journal
/// (<c>change_membership</c>), authorized by the replicated administrator predicate like any other mutation. Passes are serialized.
/// Closing the client retires the manager after any running pass.
/// </summary>
public sealed class PlacementManager : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _disposed;

    private PlacementManager(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>
    /// <paramref name="topologyNode"/> non-null reads that node's own shared view (it must have joined a swarm with
    /// <see cref="Topology.EnableSharedAsync"/>); null asks <paramref name="upcalls"/> kind 17 (register it with <see cref="PlacementUpcalls.ServePlacement"/>).
    /// </summary>
    public static PlacementManager Create(CoordinationClient client, Upcalls upcalls, Node? topologyNode = null)
    {
        ArgumentNullException.ThrowIfNull(client);
        ArgumentNullException.ThrowIfNull(upcalls);
        CoordOps.Check(Native.aster_coordination_placement_manager_new(client.Runtime.Handle, client.NativeHandle, upcalls.NativeHandle,
            topologyNode?.Handle.Value ?? 0, out ulong h), "aster_coordination_placement_manager_new");
        return new PlacementManager(client.Runtime, h);
    }

    /// <summary>
    /// Runs one pass. A provider failure (error, timeout, closed table, malformed answer, a seed for the wrong node) fails the pass with
    /// <see cref="CoordinationException"/> (kind <c>Other</c>); it never fails open. Cancelling the token abandons the wait, not the pass.
    /// </summary>
    public async Task<PlacementReport> ReconcileAsync(CancellationToken cancellationToken = default)
    {
        ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
        CoordOps.Check(Native.aster_coordination_placement_reconcile(_runtime.Handle, _handle, 0, out ulong op), "aster_coordination_placement_reconcile");
        var (_, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationResult, "aster_coordination_placement_reconcile", cancellationToken).ConfigureAwait(false);
        return CoordinationCodec.DecodePlacementReport(data);
    }

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0)
            Native.aster_coordination_placement_manager_free(_runtime.Handle, _handle);
    }
}
