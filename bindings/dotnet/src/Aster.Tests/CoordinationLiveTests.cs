using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Aster.Coordination;
using Aster.Lease;
using Aster.Upcall;

namespace Aster.Tests;

/// <summary>
/// A real in-process voter group, hosted by the TypeScript suite's fixture binary (<c>aster-coordination-fixture</c>, a Rust-only voter, plan D7).
/// Build it once: <c>with-kache cargo build -p aster-transport-napi --features coordination-fixture --bin aster-coordination-fixture</c>
/// (or point <c>ASTER_COORDINATION_FIXTURE</c> at it). Without the binary the live tests are skipped; the codec vectors always run.
/// </summary>
public sealed class CoordinationFixture : IAsyncLifetime
{
    public static string? BinaryPath()
    {
        string? env = Environment.GetEnvironmentVariable("ASTER_COORDINATION_FIXTURE");
        if (!string.IsNullOrEmpty(env)) return File.Exists(env) ? env : null;
        string exe = OperatingSystem.IsWindows() ? "aster-coordination-fixture.exe" : "aster-coordination-fixture";
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null)
        {
            foreach (string profile in new[] { "debug", "release" })
            {
                string p = Path.Combine(dir.FullName, "target", profile, exe);
                if (File.Exists(p)) return p;
            }
            dir = dir.Parent;
        }
        return null;
    }

    private Process? _proc;
    private string _dir = "";

    public byte[] GroupId { get; private set; } = [];
    public byte[] RootSecret { get; private set; } = [];
    public string Ticket { get; private set; } = "";
    public Node Root { get; private set; } = null!;

    // Directory.CreateDirectory(path) requests 0777 minus the umask; a container
    // with umask 000 would hand the voter a world-writable store, which the
    // production open policy refuses. Ask for 0700 explicitly.
    private static void CreateOwnerOnly(string path)
    {
        if (OperatingSystem.IsWindows()) Directory.CreateDirectory(path);
        else Directory.CreateDirectory(path, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);
    }

    public string NewJournalDir()
    {
        string d = Path.Combine(_dir, "journal-" + Guid.NewGuid().ToString("N"));
        CreateOwnerOnly(d);
        return d;
    }

    public async Task<Node> StartNodeAsync(byte[]? secret = null)
        => await Node.StartAsync(new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0", SecretKey = secret });

    public CoordinationClientOptions Options(string? journal = null, bool readOnly = false)
        => new() { GroupId = GroupId, SeedTickets = [Ticket], JournalDir = readOnly ? null : journal ?? NewJournalDir(), ReadOnly = readOnly };

    public async Task InitializeAsync()
    {
        string? bin = BinaryPath();
        if (bin == null) return;
        _dir = Path.Combine(Path.GetTempPath(), "aster-dotnet-coord-" + Guid.NewGuid().ToString("N"));
        CreateOwnerOnly(_dir);
        _proc = Process.Start(new ProcessStartInfo(bin, $"\"{_dir}\" 1 0xC7")
        {
            RedirectStandardOutput = true,
            RedirectStandardInput = true,
            UseShellExecute = false,
        })!;
        string? line = await _proc.StandardOutput.ReadLineAsync().WaitAsync(TimeSpan.FromSeconds(120));
        using var doc = JsonDocument.Parse(line!);
        GroupId = Convert.FromHexString(doc.RootElement.GetProperty("groupId").GetString()!);
        RootSecret = Convert.FromHexString(doc.RootElement.GetProperty("rootSecret").GetString()!);
        Ticket = doc.RootElement.GetProperty("voters")[0].GetProperty("ticket").GetString()!;
        Root = await StartNodeAsync(RootSecret);
    }

    public async Task DisposeAsync()
    {
        if (Root != null) await Root.DisposeAsync();
        if (_proc != null)
        {
            try { _proc.StandardInput.Close(); } catch (Exception) { }
            if (!_proc.WaitForExit(15000)) _proc.Kill(true);
        }
        try { if (_dir.Length > 0) Directory.Delete(_dir, true); } catch (Exception) { }
    }
}

/// <summary>A fact that runs only when the coordination fixture binary has been built.</summary>
public sealed class LiveFactAttribute : FactAttribute
{
    public LiveFactAttribute()
    {
        if (CoordinationFixture.BinaryPath() == null)
            Skip = "aster-coordination-fixture is not built (see CoordinationFixture)";
    }
}

public class CoordinationLiveTests(CoordinationFixture fx) : IClassFixture<CoordinationFixture>
{
    private static readonly TimeSpan T = TimeSpan.FromSeconds(60);

    private static ResourceKey Key(string name) => new("dotnet-" + Guid.NewGuid().ToString("N")[..8], name);

    private static byte[] Bytes(string s) => Encoding.UTF8.GetBytes(s);

    /// <summary>ROOT's client: it administers the group (grants, admins, membership, status) but holds no lease or read rights itself.</summary>
    private Task<CoordinationClient> OpenRoot(string? journal = null) => CoordinationClient.OpenAsync(fx.Root, fx.Options(journal)).WaitAsync(T);

    /// <summary>Reopen over a journal whose previous client is closing: the lock is released when its last operation ends.</summary>
    private async Task<CoordinationClient> Reopen(Node node, string journal)
    {
        var end = DateTime.UtcNow + T;
        while (true)
        {
            try { return await CoordinationClient.OpenAsync(node, fx.Options(journal)).WaitAsync(T); }
            catch (CoordinationException e) when (e.Kind == CoordinationErrorKind.JournalLocked && DateTime.UtcNow < end) { await Task.Delay(50); }
        }
    }

    /// <summary>An actor node with its own client, granted all five rights (31) on <c>keys</c> by the ROOT client.</summary>
    private sealed class Worker : IAsyncDisposable
    {
        public Node Node = null!;
        public CoordinationClient Client = null!;
        public string Journal = "";
        public async ValueTask DisposeAsync() { Client.Dispose(); await Node.DisposeAsync(); }
    }

    private async Task<Worker> WorkerFor(CoordinationClient root, params ResourceKey[] keys)
    {
        var w = new Worker { Node = await fx.StartNodeAsync(), Journal = fx.NewJournalDir() };
        w.Client = await CoordinationClient.OpenAsync(w.Node, fx.Options(w.Journal)).WaitAsync(T);
        foreach (var key in keys) await GrantAsync(root, key, w.Client.LocalNodeId);
        return w;
    }

    private static async Task GrantAsync(CoordinationClient root, ResourceKey key, byte[] node)
    {
        var acl = await root.InspectAclAsync(key).WaitAsync(T);
        await Do(await root.SetGrantAsync(key, node, 31, acl.AclRevision).WaitAsync(T));
    }

    private static async Task<MutationReply> Do(ResolvedMutation r)
    {
        var reply = r.Reply;
        await r.AcknowledgeAsync().WaitAsync(T);
        return reply;
    }

    [LiveFact]
    public async Task Open_Close_Ids_And_The_Journal_Lock()
    {
        string journal = fx.NewJournalDir();
        var a = await OpenRoot(journal);
        Assert.Equal(fx.GroupId, a.GroupId);
        Assert.Equal(Convert.FromHexString(fx.Root.NodeId()), a.LocalNodeId);
        Assert.Equal(16, (await a.ClientIdAsync().WaitAsync(T)).Length);

        // A second durable open over the same journal is refused with the coordination kind.
        var locked = await Assert.ThrowsAsync<CoordinationException>(() => CoordinationClient.OpenAsync(fx.Root, fx.Options(journal)).WaitAsync(T));
        Assert.Equal(CoordinationErrorKind.JournalLocked, locked.Kind);

        // A read-only client needs no journal and reads; everything journaled is unsupported on it.
        await using (var ro = await CoordinationClient.OpenAsync(fx.Root, fx.Options(readOnly: true)).WaitAsync(T))
        {
            Assert.True(ro.ReadOnly);
            Assert.Single((await ro.GroupStatusAsync().WaitAsync(T)).Voters);
            var e = await Assert.ThrowsAsync<IrohException>(() => ro.AcquireAsync(Key("k"), TimeSpan.FromSeconds(30)).WaitAsync(T));
            Assert.Equal(Status.Unsupported, e.Status);
        }

        await a.DisposeAsync();
        // Closed: new calls are refused with Closed.
        var closed = await Assert.ThrowsAsync<CoordinationException>(() => a.GroupStatusAsync());
        Assert.Equal(CoordinationErrorKind.Closed, closed.Kind);
        await a.DisposeAsync(); // idempotent
        await using var again = await Reopen(fx.Root, journal);

        // Bad options never reach native code.
        await Assert.ThrowsAsync<ArgumentException>(() => CoordinationClient.OpenAsync(fx.Root, new CoordinationClientOptions { GroupId = new byte[5], JournalDir = journal }));
        await Assert.ThrowsAsync<ArgumentException>(() => CoordinationClient.OpenAsync(fx.Root, new CoordinationClientOptions { GroupId = fx.GroupId }));
    }

    [LiveFact]
    public async Task Mutation_Resolves_And_Must_Be_Acknowledged_Before_The_Next()
    {
        await using var root = await OpenRoot();
        var key = Key("tree/1");
        await using var worker = await WorkerFor(root, key);
        var c = worker.Client;
        var resolved = await c.AcquireAsync(key, TimeSpan.FromSeconds(30)).WaitAsync(T);
        Assert.False(resolved.Reply.IsRejected);
        Assert.True(resolved.Reply.Snapshot!.Held);
        Assert.Equal(c.LocalNodeId, resolved.Reply.Snapshot.Holder);
        var fence = new CoordFence(c.GroupId, key, resolved.Reply.Snapshot.Epoch, c.LocalNodeId);

        // The delivery contract: the next mutation is refused until this result is acknowledged.
        var awaiting = await Assert.ThrowsAsync<CoordinationException>(() => c.RenewAsync(fence).WaitAsync(T));
        Assert.Equal(CoordinationErrorKind.AwaitingAck, awaiting.Kind);
        Assert.Equal(Status.InvalidArgument, awaiting.Status);

        await resolved.AcknowledgeAsync().WaitAsync(T);
        await Assert.ThrowsAsync<InvalidOperationException>(() => resolved.AcknowledgeAsync());

        var read = await c.ReadLinearizableAsync(key).WaitAsync(T);
        Assert.True(read.Held);
        Assert.Equal(fence.Epoch, read.Epoch);

        // CAS then release-with-value, each acknowledged.
        var cas = await Do(await c.CompareAndSetAsync(fence, read.ValueRevision, Bytes("v1")).WaitAsync(T));
        Assert.False(cas.IsRejected);
        var read2 = await c.ReadLinearizableAsync(key).WaitAsync(T);
        Assert.Equal(Bytes("v1"), read2.Value);
        Assert.True(read2.ValueRevision > read.ValueRevision);

        // A stale expected revision is a committed typed rejection: a result, not an exception.
        var rej = await Do(await c.CompareAndSetAsync(fence, read.ValueRevision, Bytes("v2")).WaitAsync(T));
        Assert.True(rej.IsRejected);

        var rel = await Do(await c.ReleaseWithValueAsync(fence, read2.ValueRevision, Bytes("final")).WaitAsync(T));
        Assert.False(rel.IsRejected);
        var freed = await c.ReadLinearizableAsync(key).WaitAsync(T);
        Assert.False(freed.Held);
        Assert.Equal(Bytes("final"), freed.Value);
    }

    [LiveFact]
    public async Task Unacknowledged_Result_Is_Redelivered_After_Reopen()
    {
        await using var root = await OpenRoot();
        var key = Key("tree/redeliver");
        await using var worker = await WorkerFor(root, key);
        string journal = worker.Journal;
        worker.Client.Dispose();
        ulong sequence;
        var c = await Reopen(worker.Node, journal);
        using (var resolved = await c.AcquireAsync(key, TimeSpan.FromSeconds(30)).WaitAsync(T))
            sequence = resolved.Reply.Sequence; // dropped without acknowledging: stays journaled
        await c.DisposeAsync();

        await using var again = await Reopen(worker.Node, journal);
        var redelivered = await again.UnacknowledgedAsync().WaitAsync(T);
        Assert.NotNull(redelivered);
        Assert.Equal(sequence, redelivered!.Reply.Sequence);
        Assert.True(redelivered.Reply.Snapshot!.Held);
        var snap = redelivered.Reply.Snapshot;
        await redelivered.AcknowledgeAsync().WaitAsync(T);
        Assert.Null(await again.UnacknowledgedAsync().WaitAsync(T));
        Assert.Null(await again.ResumeAsync().WaitAsync(T)); // nothing unresolved
        // The acknowledged journal accepts the next mutation.
        await Do(await again.ReleaseAsync(new CoordFence(again.GroupId, key, snap.Epoch, again.LocalNodeId)).WaitAsync(T));
    }

    [LiveFact]
    public async Task Reads_Cover_Every_Read_Op()
    {
        await using var root = await OpenRoot();
        var key = Key("tree/reads");
        await using var worker = await WorkerFor(root, key);
        var c = worker.Client;

        var status = await root.GroupStatusAsync().WaitAsync(T);
        Assert.Single(status.Voters);
        Assert.False(string.IsNullOrEmpty(status.AuthorityDomain));
        Assert.Equal(32, status.GenesisHash.Length);
        Assert.Equal(fx.Root.NodeId(), Convert.ToHexString(status.LocalNodeId).ToLowerInvariant() == fx.Root.NodeId() ? fx.Root.NodeId() : fx.Root.NodeId());

        var admins = await root.InspectAdminsAsync().WaitAsync(T);
        Assert.NotNull(admins.Administrators);
        var acl = await root.InspectAclAsync(key).WaitAsync(T);
        Assert.Equal(key, acl.Resource);
        Assert.Contains(acl.Grants, g => g.SubjectNodeId.AsSpan().SequenceEqual(c.LocalNodeId) && g.Rights == 31);
        var membership = await root.MembershipStatusAsync().WaitAsync(T);
        Assert.NotNull(membership);

        using (var r = await c.AcquireAsync(key, TimeSpan.FromSeconds(30)).WaitAsync(T)) await r.AcknowledgeAsync().WaitAsync(T);
        var audit = await root.AuditStreamAsync(null, 100).WaitAsync(T);
        Assert.NotEmpty(audit);
        Assert.Contains(audit, a => a.Resource != null && a.Resource.Equals(key));
        await Assert.ThrowsAsync<ArgumentOutOfRangeException>(() => root.AuditStreamAsync(null, 0));

        byte[] me = await c.ClientIdAsync().WaitAsync(T);
        var probe = await root.ProbeClientAsync(c.LocalNodeId, me).WaitAsync(T);
        Assert.True(probe.Exists);
        Assert.False(probe.Retired);

        var export = await root.RecoveryExportAsync().WaitAsync(T);
        Assert.Equal((ulong)export.Body.Length, export.TotalLen);
        string path = Path.Combine(fx.NewJournalDir(), "recovery.export");
        var manifest = await root.RecoveryExportToFileAsync(path).WaitAsync(T);
        Assert.True(File.Exists(path));
        Assert.Equal(export.Blake3Checksum.Length, manifest.Blake3Checksum.Length);
        Assert.Equal(manifest.TotalLen, (ulong)new FileInfo(path).Length);
    }

    [LiveFact]
    public async Task Acl_Refusal_Is_Permission_Denied_And_The_Request_Stays_Unresolved()
    {
        await using var other = await fx.StartNodeAsync();
        var journal = fx.NewJournalDir();
        await using var c = await CoordinationClient.OpenAsync(other, fx.Options(journal)).WaitAsync(T);
        var originalId = await c.ClientIdAsync().WaitAsync(T);
        var key = Key("tree/denied");
        var e = await Assert.ThrowsAsync<CoordinationException>(() => c.AcquireAsync(key, TimeSpan.FromSeconds(30)).WaitAsync(T));
        Assert.Equal(CoordinationErrorKind.Rpc, e.Kind);
        Assert.Equal(7u, e.RpcCode);
        Assert.Equal(Status.PermissionDenied, e.Status);
        // A denied mutation stays journaled as unresolved (plan Q2): the contract says resume first.
        var next = await Assert.ThrowsAsync<CoordinationException>(() => c.AcquireAsync(Key("tree/other"), TimeSpan.FromSeconds(30)).WaitAsync(T));
        Assert.Equal(CoordinationErrorKind.Unresolved, next.Kind);
        // A read without a READ grant is refused the same way.
        var read = await Assert.ThrowsAsync<CoordinationException>(() => c.ReadLinearizableAsync(key).WaitAsync(T));
        Assert.Equal((CoordinationErrorKind.Rpc, 7u, Status.PermissionDenied), (read.Kind, read.RpcCode, read.Status));
        await c.DisposeAsync();
        await using var root = await OpenRoot();
        await GrantAsync(root, key, Convert.FromHexString(other.NodeId()));
        await using var reopened = await CoordinationClient.OpenAsync(other, fx.Options(journal)).WaitAsync(T);
        Assert.Equal(originalId, await reopened.ClientIdAsync().WaitAsync(T));
        using var resumed = await reopened.ResumeAsync().WaitAsync(T);
        Assert.NotNull(resumed);
        Assert.True(resumed.Reply.Snapshot!.Held);
        Assert.Equal(originalId, resumed.Reply.ClientId);
        using var replay = await reopened.ResumeAsync().WaitAsync(T);
        Assert.NotNull(replay);
        Assert.Equal(resumed.Reply.Sequence, replay.Reply.Sequence);
        Assert.Equal(resumed.Reply.RequestHash, replay.Reply.RequestHash);
        await resumed.AcknowledgeAsync().WaitAsync(T);
        var snapshot = resumed.Reply.Snapshot;
        using var released = await reopened.ReleaseAsync(new CoordFence(reopened.GroupId, key, snapshot.Epoch, reopened.LocalNodeId)).WaitAsync(T);
        Assert.Equal(resumed.Reply.Sequence + 1, released.Reply.Sequence);
        Assert.False(released.Reply.Snapshot!.Held);
        await released.AcknowledgeAsync().WaitAsync(T);
    }

    [LiveFact]
    public async Task Lease_Lifecycle_Granted_Denied_Cas_Release()
    {
        await using var root = await OpenRoot();
        var key = Key("tree/lease");
        await using var w1 = await WorkerFor(root, key);
        await using var w2 = await WorkerFor(root, key);

        var granted = Assert.IsType<CoordinationLeaseAcquire.Granted>(await w1.Client.AcquireLeaseAsync(key, new LeaseTiming(TimeSpan.FromSeconds(20))).WaitAsync(T));
        using var lease = granted.Lease;
        Assert.Equal(w1.Client.LocalNodeId, lease.GrantedFence.Holder);
        Assert.Equal(lease.GrantedFence.Epoch, lease.CurrentFence()!.Epoch);
        Assert.Equal(LeaseStatus.Held, lease.CurrentStatus());

        // Held by someone else: denied, with the holder.
        var denied = Assert.IsType<CoordinationLeaseAcquire.Denied>(await w2.Client.AcquireLeaseAsync(key).WaitAsync(T));
        Assert.Equal(DenyReason.HeldByOther, denied.Reason);
        Assert.NotNull(denied.Snapshot.Holder);

        var read = await w1.Client.ReadLinearizableAsync(key).WaitAsync(T);
        var cas = await lease.CompareAndSetAsync(read.ValueRevision, Bytes("leased")).WaitAsync(T);
        Assert.False(cas.IsRejected);
        Assert.Equal(Bytes("leased"), (await w1.Client.ReadLinearizableAsync(key).WaitAsync(T)).Value);

        // Release is not prompt in general, but here nothing is in flight.
        await lease.ReleaseAsync().WaitAsync(T);
        Assert.Null(lease.CurrentFence());
        Assert.Equal(LeaseStatus.Released, lease.CurrentStatus());
        await Assert.ThrowsAsync<InvalidOperationException>(() => lease.ReleaseAsync());

        // Free again: the other worker can take it, and release it with a final value.
        var second = Assert.IsType<CoordinationLeaseAcquire.Granted>(await w2.Client.AcquireLeaseAsync(key).WaitAsync(T));
        using var l2 = second.Lease;
        var rv = (await w2.Client.ReadLinearizableAsync(key).WaitAsync(T)).ValueRevision;
        var fin = await l2.ReleaseWithValueAsync(rv, Bytes("bye")).WaitAsync(T);
        Assert.False(fin.IsRejected);
        Assert.Equal(Bytes("bye"), (await w2.Client.ReadLinearizableAsync(key).WaitAsync(T)).Value);
    }

    [LiveFact]
    public async Task Membership_Admin_And_Retire_Mutations_Round_Trip()
    {
        await using var c = await OpenRoot();
        var status = await c.GroupStatusAsync().WaitAsync(T);
        // set_admin: enable then disable a made-up node; each is a journaled, acknowledged mutation.
        byte[] target = Enumerable.Repeat((byte)0x42, 32).ToArray();
        var admins = await c.InspectAdminsAsync().WaitAsync(T);
        var on = await Do(await c.SetAdminAsync(target, true, admins.AdminRevision).WaitAsync(T));
        Assert.False(on.IsRejected);
        var admins2 = await c.InspectAdminsAsync().WaitAsync(T);
        Assert.Contains(admins2.Administrators, a => a.AsSpan().SequenceEqual(target));
        var off = await Do(await c.SetAdminAsync(target, false, admins2.AdminRevision).WaitAsync(T));
        Assert.False(off.IsRejected);

        // change_membership to the same single voter is accepted or rejected by the group, but is always a resolved result.
        var opId = Enumerable.Repeat((byte)0x0c, 16).ToArray();
        var change = await Do(await c.ChangeMembershipAsync(opId, status.MembershipLogId, status.Voters, []).WaitAsync(T));
        Assert.NotNull(change);
        // Retiring a client that never existed answers with a reply, not a hang.
        Assert.NotNull(await c.RetireOtherClientAsync(Enumerable.Repeat((byte)0x0e, 16).ToArray()).WaitAsync(T));
    }

    // ── placement ────────────────────────────────────────────────────────────

    private sealed class FuncPolicy(Func<CancellationToken, ValueTask<PlacementPolicy>> f) : IPlacementPolicySource
    {
        public ValueTask<PlacementPolicy> GetPolicyAsync(CancellationToken ct) => f(ct);
    }

    private sealed class FuncProvisioner(Func<ProvisionRequest, CancellationToken, ValueTask<MemberSeed>> f) : IPlacementProvisioner
    {
        public ValueTask<MemberSeed> ProvisionAsync(ProvisionRequest r, CancellationToken ct) => f(r, ct);
    }

    private sealed class FuncTopology(Func<ValueTask<TopologySnapshot>> f) : ITopologySource
    {
        public ValueTask<TopologySnapshot> GetTopologyAsync(CancellationToken ct) => f();
    }

    private static PlacementPolicy PolicyFor(GroupStatus status, IEnumerable<byte[]> eligible, IEnumerable<byte[]> excluded)
        => new(1, Enumerable.Repeat((byte)0x44, 32).ToArray(), status.AuthorityPublicKey, status.AuthorityDomain, 1, 0,
            eligible.OrderBy(x => Convert.ToHexString(x), StringComparer.Ordinal).Select(id => new PlacementCandidate(id, null)).ToList(), [],
            excluded.OrderBy(x => Convert.ToHexString(x), StringComparer.Ordinal).ToList(), 0, 60_000, false);

    private static TopologySnapshot ViewOf(IEnumerable<byte[]> ids)
    {
        var hex = ids.Select(i => Convert.ToHexString(i).ToLowerInvariant()).OrderBy(x => x, StringComparer.Ordinal).ToList();
        var far = new List<FarPair>();
        for (int i = 0; i < hex.Count; i++) for (int j = i + 1; j < hex.Count; j++) far.Add(new FarPair(hex[i], hex[j]));
        return new TopologySnapshot(hex.Select(h => new ClusterView([h], h, [h])).ToList(), far, 1);
    }

    [LiveFact]
    public async Task Placement_With_Dotnet_Policy_And_Provisioner()
    {
        await using var c = await OpenRoot();
        await using var up = Upcalls.Create(fx.Root.Runtime);
        var status = await c.GroupStatusAsync().WaitAsync(T);
        byte[] voter = status.Voters[0];
        byte[] newcomer = Enumerable.Repeat((byte)0x7a, 32).ToArray();
        var kinds = new ConcurrentQueue<string>();
        PlacementPolicy policy = PolicyFor(status, [voter], []);
        Func<ProvisionRequest, CancellationToken, ValueTask<MemberSeed>> provision = (_, _) => throw new InvalidOperationException("not expected");
        up.ServePlacement(
            new FuncPolicy(_ => { kinds.Enqueue("policy"); return new ValueTask<PlacementPolicy>(policy); }),
            new FuncProvisioner((r, ct) => { kinds.Enqueue("provision"); return provision(r, ct); }),
            new FuncTopology(() => { kinds.Enqueue("topology"); return new ValueTask<TopologySnapshot>(ViewOf([voter, newcomer])); }));
        using var manager = PlacementManager.Create(c, up);

        // 1. Roles already satisfy the policy.
        var report = await manager.ReconcileAsync().WaitAsync(T);
        Assert.Equal("current roles already satisfy policy", report.Reason);
        Assert.Equal(1ul, report.PolicyRevision);
        Assert.Equal(policy.Digest, report.PolicyDigest);
        Assert.Equal(new[] { voter }, report.CurrentVoters);
        Assert.Contains("policy", kinds);
        Assert.Contains("topology", kinds);
        Assert.DoesNotContain("provision", kinds);

        // 2. A candidate without seed material goes to the provisioner with the idempotency tuple; its failure fails the pass.
        policy = PolicyFor(status, [voter, newcomer], [voter]);
        ProvisionRequest? asked = null;
        provision = (r, _) => { asked = r; throw new InvalidOperationException("no capacity in that zone"); };
        var failed = await Assert.ThrowsAsync<CoordinationException>(() => manager.ReconcileAsync().WaitAsync(T));
        Assert.Contains("no capacity in that zone", failed.Message);
        Assert.NotNull(asked);
        Assert.Equal(newcomer, asked!.NodeId);
        Assert.Equal(policy.Digest, asked.PolicyDigest);
        Assert.Equal(c.GroupId, asked.GroupId);

        // 3. A provisioner answering for a different node fails the pass.
        provision = (_, _) => new ValueTask<MemberSeed>(new MemberSeed(Enumerable.Repeat((byte)0x7b, 32).ToArray(), "t", null));
        var wrong = await Assert.ThrowsAsync<CoordinationException>(() => manager.ReconcileAsync().WaitAsync(T));
        Assert.Contains("different node", wrong.Message);

        // Never fails open: the membership did not change.
        Assert.Equal(new[] { voter }, (await c.GroupStatusAsync().WaitAsync(T)).Voters);
    }

    [LiveFact]
    public async Task Placement_Provider_Failures_Fail_The_Pass_Never_Open()
    {
        await using var c = await OpenRoot();
        await using var up = Upcalls.Create(fx.Root.Runtime);
        var status = await c.GroupStatusAsync().WaitAsync(T);
        byte[] voter = status.Voters[0];
        bool topologyError = true;
        var cancelReason = new TaskCompletionSource<UpcallCancelReason>(TaskCreationOptions.RunContinuationsAsynchronously);
        up.Handle(UpcallKind.TopologySnapshot, (_, _) => topologyError
            ? throw new InvalidOperationException("topology offline")
            : new ValueTask<byte[]?>(Ucv1.EncodeTopologySnapshot(ViewOf([voter]))));
        up.Handle(UpcallKind.PlacementPolicy, (_, _) => new ValueTask<byte[]?>(CoordinationCodec.EncodePlacementPolicy(PolicyFor(status, [voter], []))));
        up.SetPolicy(UpcallKind.PlacementPolicy, TimeSpan.FromMilliseconds(400), HookFailureMode.FailOpen); // fail-open must not matter
        using var manager = PlacementManager.Create(c, up);

        var e = await Assert.ThrowsAsync<CoordinationException>(() => manager.ReconcileAsync().WaitAsync(T));
        Assert.Contains("topology offline", e.Message);

        // The topology is fine now; a malformed policy answer fails the pass.
        topologyError = false;
        up.Handle(UpcallKind.PlacementPolicy, (_, _) => new ValueTask<byte[]?>(new byte[] { 0xBD, 0x01, 0xFF }));
        await Assert.ThrowsAsync<CoordinationException>(() => manager.ReconcileAsync().WaitAsync(T));

        // No handler for the kind: UNIMPLEMENTED fails the pass.
        await using var bare = Upcalls.Create(fx.Root.Runtime);
        using var bareManager = PlacementManager.Create(c, bare);
        await Assert.ThrowsAsync<CoordinationException>(() => bareManager.ReconcileAsync().WaitAsync(T));

        // A timeout cancels the handler (CANCEL timeout) and fails the pass.
        up.Handle(UpcallKind.PlacementPolicy, async (_, ctx) =>
        {
            try { await Task.Delay(TimeSpan.FromSeconds(10), ctx.Cancellation); }
            catch (OperationCanceledException) { cancelReason.TrySetResult(ctx.CancelReason); throw; }
            return null;
        });
        await Assert.ThrowsAsync<CoordinationException>(() => manager.ReconcileAsync().WaitAsync(T));
        Assert.Equal(UpcallCancelReason.Timeout, await cancelReason.Task.WaitAsync(T));

        // Nothing changed.
        Assert.Equal(new[] { voter }, (await c.GroupStatusAsync().WaitAsync(T)).Voters);
    }

    [LiveFact]
    public async Task Close_With_Operations_In_Flight_Is_Orderly()
    {
        await using var root = await OpenRoot();
        var key = Key("tree/inflight");
        var worker = await WorkerFor(root, key);
        var lease = Assert.IsType<CoordinationLeaseAcquire.Granted>(await worker.Client.AcquireLeaseAsync(key, new LeaseTiming(TimeSpan.FromSeconds(20))).WaitAsync(T)).Lease;
        await using var up = Upcalls.Create(fx.Root.Runtime);
        var status = await root.GroupStatusAsync().WaitAsync(T);
        byte[] voter = status.Voters[0];
        var parked = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var gate = new TaskCompletionSource();
        up.Handle(UpcallKind.PlacementPolicy, async (_, ctx) =>
        {
            parked.TrySetResult();
            await gate.Task.WaitAsync(ctx.Cancellation);
            return CoordinationCodec.EncodePlacementPolicy(PolicyFor(status, [voter], []));
        });
        up.Handle(UpcallKind.TopologySnapshot, (_, _) => new ValueTask<byte[]?>(Ucv1.EncodeTopologySnapshot(ViewOf([voter]))));
        up.SetPolicy(UpcallKind.PlacementPolicy, TimeSpan.FromSeconds(30), HookFailureMode.FailClosed);
        using var manager = PlacementManager.Create(root, up);

        var pass = manager.ReconcileAsync();            // parked on its policy upcall
        await parked.Task.WaitAsync(T);
        var cas = lease.CompareAndSetAsync(0, Bytes("x")); // one journaled op in flight on the worker's client
        worker.Client.Dispose();                         // close with an op in flight: returns at once
        await root.DisposeAsync();                       // and with a placement pass in flight

        // Handles of a closed client are inert and refused at once.
        var closedCas = await Assert.ThrowsAsync<CoordinationException>(() => lease.CompareAndSetAsync(0, Bytes("y")).WaitAsync(T));
        Assert.Equal(CoordinationErrorKind.Closed, closedCas.Kind);
        Assert.Null(lease.CurrentFence());
        var closedPass = await Assert.ThrowsAsync<CoordinationException>(() => manager.ReconcileAsync().WaitAsync(T));
        Assert.Equal(CoordinationErrorKind.Closed, closedPass.Kind);

        // In-flight operations complete normally (or fail with a coordination error); nothing hangs.
        gate.SetResult();
        try { await pass.WaitAsync(T); } catch (CoordinationException) { }
        try { await cas.WaitAsync(T); } catch (CoordinationException) { }

        // The lock is released when the last operation ends: the worker's journal reopens.
        await using var again = await Reopen(worker.Node, worker.Journal);
        lease.Dispose();
        await worker.Node.DisposeAsync();
    }
}
