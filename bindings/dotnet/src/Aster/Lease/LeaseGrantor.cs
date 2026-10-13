using System.Text.Json;
using Aster.Lease;
using Aster.Rpc;

namespace Aster;

/// <summary>
/// Serves <c>aster.lease.Grantor</c> from an <see cref="AsterServer"/>
/// (<see cref="AsterServerOptions.LeaseGrantor"/>; C ABI 5.1 <c>aster_server_add_lease_grantor</c>,
/// Rust <c>aster::rpc::lease::LeaseGrantor</c>): a designated serialization point other nodes
/// acquire, renew and release leases through (<see cref="AsterClient.LeaseGrantorAsync(NodeAddrInfo, System.Threading.CancellationToken)"/>).
/// The grantor binds every operation to the caller's authenticated node id, never a request
/// field, so a node can only hold, renew or release leases as itself. A dead grantor stops
/// grants and renewals (holders run to their self-limit and stop); there is no failover.
/// </summary>
public sealed record LeaseGrantorOptions
{
    /// <summary>
    /// The serialization point, implemented in .NET (for example over a database) and served on
    /// an upcall table the server owns. Null = Rust's in-memory reference serializer.
    /// </summary>
    public ILeaseSerializer? Serializer { get; init; }

    /// <summary>
    /// Gates every method (Gate 3); null = none, so admission (Gate 0, e.g. Enrollment) is the
    /// boundary. Without it any admitted caller can burn epochs (availability, never correctness).
    /// </summary>
    public CapabilityRequirement? Requires { get; init; }

    /// <summary>Gates <c>revoke</c>; null = <c>CapabilityRequirement.Role("operator")</c>.</summary>
    public CapabilityRequirement? RevokeRequires { get; init; }

    /// <summary>Leaves <c>revoke</c> gated only by <see cref="Requires"/>.</summary>
    public bool RevokeUngated { get; init; }

    internal static byte[] Json(CapabilityRequirement r)
        => JsonSerializer.SerializeToUtf8Bytes(new { kind = r.WireKind, roles = r.Roles });

    /// <summary><c>requires_json</c> and <c>revoke_requires_json</c> (empty = default, <c>null</c> = none).</summary>
    internal (byte[] Requires, byte[] Revoke) Requirements()
    {
        byte[] requires = Requires is null ? [] : Json(Requires);
        byte[] revoke = RevokeUngated ? "null"u8.ToArray() : RevokeRequires is null ? [] : Json(RevokeRequires);
        return (requires, revoke);
    }
}
