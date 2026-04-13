using System.Text;
using System.Text.Json;

namespace Aster.Registry;

/// <summary>
/// Async doc-backed registry operations (§11.8). These complement the
/// synchronous filter/rank entry points in <see cref="Registry"/> by going
/// through the bridge tokio runtime: each call submits an FFI op, then
/// awaits the matching event (kinds 80-84) on the runtime event pump.
///
/// Round-robin rotation and stale-seq filtering are persistent on the
/// Rust bridge, so callers do not need to maintain their own state.
/// </summary>
public static partial class Registry
{
    /// <summary>
    /// Run the full resolve pipeline (pointer lookup, list_leases, monotonic
    /// seq filter, mandatory filters, rank) for the given options against
    /// the given registry doc. Returns the winning lease, or null if no
    /// candidate survived.
    /// </summary>
    public static async Task<EndpointLease?> ResolveAsync(
        Doc doc, ResolveOptions opts, CancellationToken ct = default)
    {
        byte[] optsJson = Encoding.UTF8.GetBytes(opts.ToJson());
        ulong opId;
        unsafe
        {
            fixed (byte* op = optsJson)
            {
                int r = Native.aster_registry_resolve(
                    doc.Runtime.Handle, doc.Handle, op, (UIntPtr)optsJson.Length,
                    user_data: 0, out opId);
                if (r != 0) throw IrohException.FromStatus(r, "aster_registry_resolve");
            }
        }
        Event ev = await doc.Runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.RegistryResolved)
            throw new IrohException($"resolve: unexpected event {(EventKind)ev.kind}");
        if (ev.status == (uint)Status.NotFound)
            return null;
        return ReadJsonPayload(doc.Runtime, ev, RegistryJson.Options) is { } json
            ? JsonSerializer.Deserialize<EndpointLease>(json, RegistryJson.Options)
            : null;
    }

    /// <summary>
    /// Publish a lease and/or an artifact in a single op. Either may be null
    /// to skip; at least one must be supplied. When publishing an artifact,
    /// <paramref name="service"/> and <paramref name="version"/> are required.
    /// <paramref name="topic"/> is optional gossip topic to broadcast the
    /// matching event on.
    /// </summary>
    public static async Task PublishAsync(
        Doc doc,
        string authorId,
        EndpointLease? lease,
        ArtifactRef? artifact,
        string service = "",
        int version = 0,
        string? channel = null,
        GossipTopic? topic = null,
        CancellationToken ct = default)
    {
        if (lease is null && artifact is null)
            throw new ArgumentException("PublishAsync requires at least one of lease or artifact");

        byte[] author = Encoding.UTF8.GetBytes(authorId);
        byte[] leaseBytes = lease is null ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(lease.ToJson());
        byte[] artifactBytes = artifact is null ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(artifact.ToJson());
        byte[] serviceBytes = Encoding.UTF8.GetBytes(service);
        byte[] channelBytes = channel is null ? Array.Empty<byte>() : Encoding.UTF8.GetBytes(channel);
        ulong topicHandle = topic?.Handle ?? 0;

        ulong opId;
        unsafe
        {
            fixed (byte* a = author)
            fixed (byte* l = leaseBytes.Length > 0 ? leaseBytes : new byte[1])
            fixed (byte* af = artifactBytes.Length > 0 ? artifactBytes : new byte[1])
            fixed (byte* s = serviceBytes.Length > 0 ? serviceBytes : new byte[1])
            fixed (byte* c = channelBytes.Length > 0 ? channelBytes : new byte[1])
            {
                int r = Native.aster_registry_publish(
                    doc.Runtime.Handle, doc.Handle,
                    a, (UIntPtr)author.Length,
                    leaseBytes.Length > 0 ? l : null, (UIntPtr)leaseBytes.Length,
                    artifactBytes.Length > 0 ? af : null, (UIntPtr)artifactBytes.Length,
                    serviceBytes.Length > 0 ? s : null, (UIntPtr)serviceBytes.Length,
                    version,
                    channelBytes.Length > 0 ? c : null, (UIntPtr)channelBytes.Length,
                    topicHandle, user_data: 0, out opId);
                if (r != 0) throw IrohException.FromStatus(r, "aster_registry_publish");
            }
        }
        Event ev = await doc.Runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.RegistryPublished)
            throw new IrohException($"publish: unexpected event {(EventKind)ev.kind}");
    }

    /// <summary>
    /// Renew an existing lease in place. Reads the current lease row, bumps
    /// lease_seq + timestamps, updates health/load, and rewrites it.
    /// Pass <c>float.NaN</c> for <paramref name="load"/> to leave it unset.
    /// </summary>
    public static async Task RenewLeaseAsync(
        Doc doc,
        string authorId,
        string service,
        string contractId,
        string endpointId,
        string health,
        float load,
        int leaseDurationS,
        GossipTopic? topic = null,
        CancellationToken ct = default)
    {
        byte[] author = Encoding.UTF8.GetBytes(authorId);
        byte[] svc = Encoding.UTF8.GetBytes(service);
        byte[] cid = Encoding.UTF8.GetBytes(contractId);
        byte[] eid = Encoding.UTF8.GetBytes(endpointId);
        byte[] hb = Encoding.UTF8.GetBytes(health);
        ulong topicHandle = topic?.Handle ?? 0;

        ulong opId;
        unsafe
        {
            fixed (byte* a = author)
            fixed (byte* s = svc)
            fixed (byte* c = cid)
            fixed (byte* e = eid)
            fixed (byte* h = hb)
            {
                int r = Native.aster_registry_renew_lease(
                    doc.Runtime.Handle, doc.Handle,
                    a, (UIntPtr)author.Length,
                    s, (UIntPtr)svc.Length,
                    c, (UIntPtr)cid.Length,
                    e, (UIntPtr)eid.Length,
                    h, (UIntPtr)hb.Length,
                    load, leaseDurationS,
                    topicHandle, user_data: 0, out opId);
                if (r != 0) throw IrohException.FromStatus(r, "aster_registry_renew_lease");
            }
        }
        Event ev = await doc.Runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.RegistryRenewed)
            throw new IrohException($"renew_lease: unexpected event {(EventKind)ev.kind}");
    }

    /// <summary>
    /// Add an author to the per-doc registry ACL writer set, persisting the
    /// updated list to the doc under <c>_aster/acl/writers</c>. Switches the
    /// ACL out of open mode if it was in open mode.
    /// </summary>
    public static Task AclAddWriterAsync(
        Doc doc, string authorId, string writerId, CancellationToken ct = default)
        => MutateAclWriter(doc, authorId, writerId, add: true, ct);

    /// <summary>
    /// Remove an author from the per-doc registry ACL writer set and persist
    /// the updated list.
    /// </summary>
    public static Task AclRemoveWriterAsync(
        Doc doc, string authorId, string writerId, CancellationToken ct = default)
        => MutateAclWriter(doc, authorId, writerId, add: false, ct);

    private static async Task MutateAclWriter(
        Doc doc, string authorId, string writerId, bool add, CancellationToken ct)
    {
        byte[] author = Encoding.UTF8.GetBytes(authorId);
        byte[] writer = Encoding.UTF8.GetBytes(writerId);
        ulong opId;
        unsafe
        {
            fixed (byte* a = author)
            fixed (byte* w = writer)
            {
                int r = add
                    ? Native.aster_registry_acl_add_writer(
                        doc.Runtime.Handle, doc.Handle, a, (UIntPtr)author.Length,
                        w, (UIntPtr)writer.Length, user_data: 0, out opId)
                    : Native.aster_registry_acl_remove_writer(
                        doc.Runtime.Handle, doc.Handle, a, (UIntPtr)author.Length,
                        w, (UIntPtr)writer.Length, user_data: 0, out opId);
                if (r != 0)
                    throw IrohException.FromStatus(r,
                        add ? "aster_registry_acl_add_writer" : "aster_registry_acl_remove_writer");
            }
        }
        Event ev = await doc.Runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.RegistryAclUpdated)
            throw new IrohException($"acl mutate: unexpected event {(EventKind)ev.kind}");
    }

    /// <summary>
    /// List the current writer set for the per-doc registry ACL. Returns an
    /// empty list when the ACL is still in open mode.
    /// </summary>
    public static async Task<List<string>> AclListWritersAsync(Doc doc, CancellationToken ct = default)
    {
        int r = Native.aster_registry_acl_list_writers(
            doc.Runtime.Handle, doc.Handle, user_data: 0, out ulong opId);
        if (r != 0) throw IrohException.FromStatus(r, "aster_registry_acl_list_writers");
        Event ev = await doc.Runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)EventKind.RegistryAclListed)
            throw new IrohException($"acl list: unexpected event {(EventKind)ev.kind}");
        string? json = ReadJsonPayload(doc.Runtime, ev, RegistryJson.Options);
        if (json is null) return new List<string>();
        return JsonSerializer.Deserialize<List<string>>(json, RegistryJson.Options)
               ?? new List<string>();
    }

    private static string? ReadJsonPayload(Runtime runtime, Event ev, JsonSerializerOptions _)
    {
        if (ev.data_ptr == IntPtr.Zero || ev.data_len == UIntPtr.Zero)
            return null;
        byte[] data = new byte[(int)ev.data_len];
        System.Runtime.InteropServices.Marshal.Copy(ev.data_ptr, data, 0, (int)ev.data_len);
        if (ev.buffer != 0) runtime.ReleaseBuffer(ev.buffer);
        return Encoding.UTF8.GetString(data);
    }
}
