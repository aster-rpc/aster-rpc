using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Aster.Lease;
using Aster.Upcall;

namespace Aster.Tests;

/// <summary>Ucv1 against conformance/vectors/upcalls.json: every valid case of a supported type round-trips byte for byte, every invalid input is refused.</summary>
public class Ucv1VectorTests
{
    /// <summary>Placement messages are not used by the C ABI (no coordination client).</summary>
    private static readonly HashSet<string> Unsupported = new() { "PlacementPolicyRequest", "PlacementPolicy", "PlacementProvisionRequest", "MemberSeed" };

    private static readonly Dictionary<string, Func<byte[], byte[]>> RoundTrip = new()
    {
        ["LeaseTransitionRequest"] = b => { var v = Ucv1.DecodeLeaseTransitionRequest(b); return Ucv1.EncodeLeaseTransitionRequest(v.Resource, v.Op); },
        ["TransitionOutcome"] = b => Ucv1.EncodeTransitionOutcome(Ucv1.DecodeTransitionOutcome(b)),
        ["LeaseSnapshotRequest"] = b => Ucv1.EncodeLeaseSnapshotRequest(Ucv1.DecodeLeaseSnapshotRequest(b)),
        ["LeaseSnapshot"] = b => Ucv1.EncodeLeaseSnapshot(Ucv1.DecodeLeaseSnapshot(b)),
        ["LeaseAuthorizeRequest"] = b => Ucv1.EncodeLeaseAuthorizeRequest(Ucv1.DecodeLeaseAuthorizeRequest(b)),
        ["FencedMutateRequest"] = b => { var v = Ucv1.DecodeFencedMutateRequest(b); return Ucv1.EncodeFencedMutateRequest(v.Fence, v.Writer, v.Op); },
        ["FencedMutateResponse"] = b => Ucv1.EncodeFencedMutateResponse(Ucv1.DecodeFencedMutateResponse(b)),
        ["TopologyAdmittedRequest"] = b => Ucv1.EncodeTopologyAdmittedRequest(Ucv1.DecodeTopologyAdmittedRequest(b)),
        ["TopologyAdmittedResponse"] = b => Ucv1.EncodeTopologyAdmittedResponse(Ucv1.DecodeTopologyAdmittedResponse(b)),
        ["TopologySnapshotRequest"] = b => { Ucv1.DecodeTopologySnapshotRequest(b); return Ucv1.EncodeTopologySnapshotRequest(); },
        ["TopologySnapshot"] = b => Ucv1.EncodeTopologySnapshot(Ucv1.DecodeTopologySnapshot(b)),
        ["ExposeAuthorizeRequest"] = b => Ucv1.EncodeExposeAuthorizeRequest(Ucv1.DecodeExposeAuthorizeRequest(b)),
        ["AdmissionConnectRequest"] = b => Ucv1.EncodeAdmissionConnectRequest(Ucv1.DecodeAdmissionConnectRequest(b)),
        ["AdmissionHandshakeRequest"] = b => Ucv1.EncodeAdmissionHandshakeRequest(Ucv1.DecodeAdmissionHandshakeRequest(b)),
        ["AdmissionHandshakeResponse"] = b => Ucv1.EncodeAdmissionHandshakeResponse(Ucv1.DecodeAdmissionHandshakeResponse(b)),
        ["ExposeHttpRequest"] = b => Ucv1.EncodeExposeHttpRequest(Ucv1.DecodeExposeHttpRequest(b)),
        ["ExposeHttpResponse"] = b => Ucv1.EncodeExposeHttpResponse(Ucv1.DecodeExposeHttpResponse(b)),
        ["ExposeHttpTrailers"] = b => Ucv1.EncodeExposeHttpTrailers(Ucv1.DecodeExposeHttpTrailers(b)),
        ["ExposeRoutePolicyRequest"] = b => Ucv1.EncodeExposeRoutePolicyRequest(Ucv1.DecodeExposeRoutePolicyRequest(b)),
        ["ExposeRouteGrant"] = b => Ucv1.EncodeExposeRouteGrant(Ucv1.DecodeExposeRouteGrant(b)),
        ["Decision"] = b => Ucv1.EncodeDecision(Ucv1.DecodeDecision(b)),
    };

    public static IEnumerable<object[]> Valid()
        => Vectors.Load("upcalls.json").GetProperty("valid").EnumerateArray()
            .Where(v => !Unsupported.Contains(v.Str("type"))).Select(v => new object[] { v.Str("name") });

    public static IEnumerable<object[]> Invalid()
        => Vectors.Load("upcalls.json").GetProperty("invalid").EnumerateArray()
            .Where(v => !Unsupported.Contains(v.Str("type"))).Select(v => new object[] { v.Str("name"), v.Str("type") });

    [Theory]
    [MemberData(nameof(Valid))]
    public void Valid_Vector_Round_Trips_Byte_For_Byte(string name)
    {
        var v = Vectors.Load("upcalls.json").GetProperty("valid").EnumerateArray().First(x => x.Str("name") == name);
        byte[] bytes = Convert.FromHexString(v.Str("hex"));
        Assert.Equal(bytes, RoundTrip[v.Str("type")](bytes));
    }

    [Theory]
    [MemberData(nameof(Invalid))]
    public void Invalid_Vector_Is_Refused(string name, string type)
    {
        var v = Vectors.Load("upcalls.json").GetProperty("invalid").EnumerateArray().First(x => x.Str("name") == name && x.Str("type") == type);
        byte[] bytes = Convert.FromHexString(v.Str("hex"));
        Assert.Throws<Ucv1DecodeException>(() => RoundTrip[type](bytes));
    }

    [Fact]
    public void Every_Supported_Type_Has_A_Vector_And_Placement_Is_The_Only_Gap()
    {
        var all = Vectors.Load("upcalls.json").GetProperty("valid").EnumerateArray().Select(v => v.Str("type")).ToHashSet();
        Assert.Equal(Unsupported.OrderBy(x => x), all.Where(Unsupported.Contains).OrderBy(x => x));
        Assert.Equal(all.Where(t => !Unsupported.Contains(t)).OrderBy(x => x), RoundTrip.Keys.OrderBy(x => x));
    }

    [Fact]
    public void Decoded_Values_Match_The_Vector_Rendering()
    {
        var valid = Vectors.Load("upcalls.json").GetProperty("valid").EnumerateArray().ToList();
        byte[] Hex(string n) => Convert.FromHexString(valid.First(x => x.Str("name") == n).Str("hex"));

        var acquire = Ucv1.DecodeLeaseTransitionRequest(Hex("lease transition: acquire"));
        Assert.Equal("tree/42", acquire.Resource);
        Assert.Equal(new LeaseOp.Acquire(new string('a', 64), 10000), acquire.Op);
        var release = Ucv1.DecodeLeaseTransitionRequest(Hex("lease transition: release (multi-byte epoch)"));
        Assert.Equal(new LeaseOp.Release(new string('a', 64), 300), release.Op);

        var get = Ucv1.DecodeExposeHttpRequest(Hex("expose http request: GET, streamed body"));
        Assert.Equal("app", get.ServiceId);
        Assert.Equal(new string('a', 64), get.PeerId);
        Assert.Equal(7ul, get.StreamId);
        Assert.Equal(2, get.Version);
        Assert.Equal("/greet?x=1", get.Uri);
        Assert.Same(HttpBody.Stream, get.Body);
        Assert.Equal(new[] { "host", "accept" }, get.Headers.Select(h => h.Name));

        var post = Ucv1.DecodeExposeHttpRequest(Hex("expose http request: POST inline, no peer, HTTP/2, binary header"));
        Assert.Null(post.PeerId);
        Assert.Equal(300ul, post.StreamId);
        Assert.Equal(new HttpBody.Inline(Encoding.UTF8.GetBytes("hello")), post.Body);
        Assert.Equal(new byte[] { 0xff, 0x00 }, post.Headers[1].Value);

        var resp = Ucv1.DecodeExposeHttpResponse(Hex("expose http response: inline, repeated header"));
        Assert.Equal(200, resp.Status);
        Assert.Equal(new[] { "set-cookie", "set-cookie" }, resp.Headers.Where(h => h.Name == "set-cookie").Select(h => h.Name));
        Assert.Equal(new[] { "a=1", "b=2" }, resp.Headers.Where(h => h.Name == "set-cookie").Select(h => h.ValueText));

        var routes = Ucv1.DecodeExposeRoutePolicyRequest(Hex("expose route policy request"));
        Assert.Equal(new ExposeRouteSpec("a.example", 443, ExposeRouteSpec.ProtocolHttp, "web"), routes.Specs[0]);
        Assert.Equal(new ExposeRouteSpec("", 5432, ExposeRouteSpec.ProtocolTcp, "db"), routes.Specs[1]);
        Assert.Equal(new uint[] { 1 }, Ucv1.DecodeExposeRouteGrant(Hex("expose route grant")));
    }

    [Fact]
    public void Kind_Status_And_Cancel_Reason_Tables_Match_The_Enums()
    {
        var doc = Vectors.Load("upcalls.json");
        foreach (var k in doc.GetProperty("kinds").EnumerateArray())
            Assert.Equal(k.Str("name").Replace("_", ""), ((UpcallKind)k.GetProperty("code").GetInt32()).ToString().ToLowerInvariant());
        foreach (var s in doc.GetProperty("statuses").EnumerateArray())
            Assert.Equal(s.Str("name"), ((UpcallStatus)s.GetProperty("code").GetInt32()).ToString().ToLowerInvariant());
        foreach (var c in doc.GetProperty("cancel_reasons").EnumerateArray())
            Assert.Equal(c.Str("name"), ((UpcallCancelReason)c.GetProperty("code").GetInt32()).ToString().ToLowerInvariant());
    }

    [Fact]
    public void Hand_Crafted_Refusals()
    {
        // Strict UTF-8, minimal varints, lowercase header names, no trailing bytes.
        Assert.Throws<Ucv1DecodeException>(() => Ucv1.DecodeLeaseSnapshotRequest(new byte[] { 0xB3, 1, 0x0A, 0xFF, 0xFE }));
        Assert.Throws<Ucv1DecodeException>(() => Ucv1.DecodeDecision(new byte[] { 0xCF, 1, 2 }));
        Assert.Throws<Ucv1DecodeException>(() => Ucv1.DecodeDecision(new byte[] { 0xCF, 1, 1, 0 }));
        Assert.Throws<Ucv1DecodeException>(() => Ucv1.DecodeDecision(new byte[] { 0xCF, 2, 1 }));
        Assert.Throws<Ucv1DecodeException>(() => Ucv1.DecodeDecision(Array.Empty<byte>()));
        // A list count far larger than the payload must not allocate.
        Assert.Throws<Ucv1DecodeException>(() => Ucv1.DecodeTopologyAdmittedRequest(new byte[] { 0xB8, 1, 0xFF, 0xFF, 0xFF, 0x7F, 0x0C }));
    }
}

/// <summary>A minimal .NET lease serialization point (the state machine of <c>aster::lease</c>).</summary>
internal sealed class DotNetSerializer : ILeaseSerializer
{
    private sealed class Row
    {
        public ulong Epoch;
        public string? Holder;
        public bool Held;
        public ulong TtlMs;
        public long ExpiresAt;
    }

    private readonly Dictionary<string, Row> _rows = new();
    private readonly object _gate = new();
    public ConcurrentQueue<LeaseOp> Seen { get; } = new();

    private static LeaseSnapshot Snap(string resource, Row r, long now)
        => new(resource, r.Epoch, r.Held ? r.Holder : null, r.Held, r.Held ? (ulong)Math.Max(0, r.ExpiresAt - now) : null);

    public ValueTask<TransitionOutcome> TransitionAsync(string resource, LeaseOp op, CancellationToken ct)
    {
        Seen.Enqueue(op);
        lock (_gate)
        {
            long now = Environment.TickCount64;
            if (!_rows.TryGetValue(resource, out var r)) _rows[resource] = r = new Row();
            bool live = r.Held && now < r.ExpiresAt;
            TransitionOutcome o;
            switch (op)
            {
                case LeaseOp.Acquire a:
                    if (live) { o = new TransitionOutcome.Denied(DenyReason.HeldByOther, Snap(resource, r, now)); break; }
                    r.Epoch++; r.Holder = a.Candidate; r.Held = true; r.TtlMs = a.TtlMs; r.ExpiresAt = now + (long)a.TtlMs;
                    o = new TransitionOutcome.Granted(new Fence(resource, r.Epoch, r.Holder));
                    break;
                case LeaseOp.Renew n:
                    if (Check(r, n.Holder, n.Epoch) is { } d1) { o = new TransitionOutcome.Denied(d1, Snap(resource, r, now)); break; }
                    r.ExpiresAt = now + (long)r.TtlMs;
                    o = new TransitionOutcome.Renewed();
                    break;
                case LeaseOp.Release l:
                    if (Check(r, l.Holder, l.Epoch) is { } d2) { o = new TransitionOutcome.Denied(d2, Snap(resource, r, now)); break; }
                    r.Epoch++; r.Held = false; r.Holder = null;
                    o = new TransitionOutcome.Released();
                    break;
                default:
                    r.Epoch++; r.Held = false; r.Holder = null;
                    o = new TransitionOutcome.Revoked();
                    break;
            }
            return new ValueTask<TransitionOutcome>(o);
        }
    }

    private static DenyReason? Check(Row r, string holder, ulong epoch)
    {
        if (!r.Held) return DenyReason.NotHeld;
        if (r.Epoch != epoch) return DenyReason.EpochMismatch;
        if (holder != r.Holder) return DenyReason.NotHolder;
        return null;
    }

    public ValueTask<LeaseSnapshot> SnapshotAsync(string resource, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_rows.TryGetValue(resource, out var r)) _rows[resource] = r = new Row();
            return new ValueTask<LeaseSnapshot>(Snap(resource, r, Environment.TickCount64));
        }
    }
}

internal sealed class FuncAuthorizer(Func<LeaseOp, CancellationToken, ValueTask<bool>> f) : ILeaseAuthorizer
{
    public ValueTask<bool> AuthorizeAsync(LeaseOp op, CancellationToken ct) => f(op, ct);
}

internal sealed class FuncFenced(Func<Fence, string, byte[], CancellationToken, ValueTask<MutateResult>> f) : IFencedResource
{
    public ValueTask<MutateResult> MutateAsync(Fence fence, string writer, byte[] op, CancellationToken ct) => f(fence, writer, op, ct);
}

internal sealed class FuncFilter(Func<IReadOnlyList<string>, CancellationToken, ValueTask<IReadOnlyList<bool>>> f) : IAdmittedFilter
{
    public ValueTask<IReadOnlyList<bool>> AdmittedAsync(IReadOnlyList<string> ids, CancellationToken ct) => f(ids, ct);
}

/// <summary>C ABI 4.1 end to end: Rust lease handles over a .NET serializer, an authorizer guarding Rust's memory serializer, a fenced resource, the topology filter, failure modes, cancel delivery, disposal.</summary>
public class UpcallTests
{
    private static readonly TimeSpan T = TimeSpan.FromSeconds(15);

    private static Task<TResult> Within<TResult>(Task<TResult> t) => t.WaitAsync(T);

    private static Task Within(Task t) => t.WaitAsync(T);

    private static async Task Eventually(Func<bool> cond, string what)
    {
        var end = DateTime.UtcNow + T;
        while (!cond())
        {
            Assert.True(DateTime.UtcNow < end, "timed out: " + what);
            await Task.Delay(10);
        }
    }

    [Fact]
    public async Task Dotnet_Serializer_Backs_Rust_Lease_Handles()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        var js = new DotNetSerializer();
        up.ServeLeaseSerializer(js);
        using var ser = NativeLeaseSerializer.Foreign(up);

        var r = await Within(ser.AcquireAsync("tree/42", "alice", new LeaseTiming(TimeSpan.FromMilliseconds(600))));
        await using var lease = Assert.IsType<AcquireResult.Granted>(r).Lease;
        Assert.Equal(new Fence("tree/42", 1, "alice"), lease.GrantedFence);
        Assert.Equal(1ul, lease.CurrentFence()!.Epoch);
        Assert.Equal(LeaseStatus.Held, lease.CurrentStatus());

        var bob = Assert.IsType<AcquireResult.Denied>(await Within(ser.AcquireAsync("tree/42", "bob", new LeaseTiming(TimeSpan.FromMilliseconds(600)))));
        Assert.Equal(DenyReason.HeldByOther, bob.Reason);
        Assert.Equal("alice", bob.Snapshot.Holder);

        // Renewals reach .NET (ttl 600 ms -> every 200 ms).
        await Eventually(() => js.Seen.Count(o => o is LeaseOp.Renew) >= 2, "renewals");
        Assert.NotNull(lease.CurrentFence());

        var snap = await Within(ser.SnapshotAsync("tree/42"));
        Assert.True(snap.Held);
        Assert.Equal("alice", snap.Holder);

        var watch = lease.WatchAsync();
        await Within(lease.ReleaseAsync());
        Assert.Equal(LeaseStatus.Released, await Within(watch));
        Assert.Null(lease.CurrentFence());
        Assert.Contains(new LeaseOp.Release("alice", 1), js.Seen);

        // Revoke fences the next holder's renewal.
        using var b = Assert.IsType<AcquireResult.Granted>(await Within(ser.AcquireAsync("tree/42", "bob", new LeaseTiming(TimeSpan.FromMilliseconds(600))))).Lease;
        Assert.True(b.GrantedFence.Epoch > 1);
        Assert.IsType<TransitionOutcome.Revoked>(await Within(ser.RevokeAsync("tree/42", "root")));
        Assert.Equal(LeaseStatus.Fenced, await Within(b.WatchAsync()));
        Assert.Null(b.CurrentFence());
    }

    [Fact]
    public async Task Authorizer_Guards_The_Memory_Serializer()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        up.ServeLeaseAuthorizer(new FuncAuthorizer((op, _) => new ValueTask<bool>(op.Actor != "mallory")));
        using (var ser = NativeLeaseSerializer.Memory(rt, up))
        {
            var m = Assert.IsType<AcquireResult.Denied>(await Within(ser.AcquireAsync("db", "mallory")));
            Assert.Equal(DenyReason.Unauthorized, m.Reason);
            using var a = Assert.IsType<AcquireResult.Granted>(await Within(ser.AcquireAsync("db", "alice"))).Lease;
        }
        using var open = NativeLeaseSerializer.Memory(rt, null);
        using var open1 = Assert.IsType<AcquireResult.Granted>(await Within(open.AcquireAsync("db", "mallory"))).Lease;
    }

    [Fact]
    public async Task Failure_Mode_Applies_When_The_Authorizer_Hangs_Or_Throws()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        var cancelled = new TaskCompletionSource<UpcallCancelReason>(TaskCreationOptions.RunContinuationsAsynchronously);
        up.Handle(UpcallKind.LeaseAuthorize, async (req, ctx) =>
        {
            var op = Ucv1.DecodeLeaseAuthorizeRequest(req);
            if (op.Actor == "thrower") throw new InvalidOperationException("boom");
            try { await Task.Delay(TimeSpan.FromSeconds(10), ctx.Cancellation); }
            catch (OperationCanceledException) { cancelled.TrySetResult(ctx.CancelReason); throw; }
            return Ucv1.EncodeDecision(true);
        });
        up.SetPolicy(UpcallKind.LeaseAuthorize, TimeSpan.FromMilliseconds(150), HookFailureMode.FailClosed);
        using var ser = NativeLeaseSerializer.Memory(rt, up);

        var d = Assert.IsType<AcquireResult.Denied>(await Within(ser.AcquireAsync("r", "slow")));
        Assert.Equal(DenyReason.Unauthorized, d.Reason);
        // Rust cancelled the timed-out upcall: the handler's token fired with reason TIMEOUT.
        Assert.Equal(UpcallCancelReason.Timeout, await Within(cancelled.Task));
        Assert.True(up.CancelCount >= 1);

        Assert.IsType<AcquireResult.Denied>(await Within(ser.AcquireAsync("r", "thrower")));

        up.SetPolicy(UpcallKind.LeaseAuthorize, TimeSpan.FromMilliseconds(150), HookFailureMode.FailOpen);
        using var l = Assert.IsType<AcquireResult.Granted>(await Within(ser.AcquireAsync("r", "thrower"))).Lease;
    }

    [Fact]
    public async Task Cancelling_The_Dotnet_Token_Abandons_The_Wait_But_Rust_Keeps_Waiting_Until_The_Policy_Timeout()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        var entered = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var reason = new TaskCompletionSource<UpcallCancelReason>(TaskCreationOptions.RunContinuationsAsynchronously);
        up.Handle(UpcallKind.LeaseTransition, async (req, ctx) =>
        {
            entered.TrySetResult();
            try { await Task.Delay(TimeSpan.FromSeconds(30), ctx.Cancellation); }
            catch (OperationCanceledException) { reason.TrySetResult(ctx.CancelReason); throw; }
            return null;
        });
        // aster_operation_cancel only suppresses the completion event: the Rust call (and so the
        // upcall) lives until the kind's timeout, which then cancels the handler (reason TIMEOUT).
        up.SetPolicy(UpcallKind.LeaseTransition, TimeSpan.FromMilliseconds(400), HookFailureMode.FailClosed);
        using var ser = NativeLeaseSerializer.Foreign(up);
        using var cts = new CancellationTokenSource();
        var acquire = ser.AcquireAsync("r", "a", null, cts.Token);
        await Within(entered.Task);
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => acquire);
        Assert.Equal(UpcallCancelReason.Timeout, await Within(reason.Task));
    }

    [Fact]
    public async Task Missing_Handler_And_Handler_Errors_Fail_The_Rust_Call()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        using var ser = NativeLeaseSerializer.Foreign(up);

        var e = await Assert.ThrowsAsync<IrohException>(() => Within(ser.AcquireAsync("r", "a")));
        Assert.Contains("not implemented", e.Message);

        up.Handle(UpcallKind.LeaseTransition, (_, _) => throw new InvalidOperationException("disk on fire"));
        e = await Assert.ThrowsAsync<IrohException>(() => Within(ser.AcquireAsync("r", "a")));
        Assert.Contains("disk on fire", e.Message);

        // Garbage in an OK answer is a decode error, never a grant.
        up.Handle(UpcallKind.LeaseTransition, (_, _) => new ValueTask<byte[]?>(new byte[] { 1, 2, 3 }));
        e = await Assert.ThrowsAsync<IrohException>(() => Within(ser.AcquireAsync("r", "a")));
        Assert.Contains("malformed", e.Message);
    }

    [Fact]
    public async Task Fenced_Resource()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        // The resource's own row: epoch 7 held by alice.
        up.ServeFencedResource(new FuncFenced((fence, writer, op, _) =>
        {
            var rej = FenceRejection.Check(7, true, "alice", fence, writer);
            if (rej != null) return new ValueTask<MutateResult>(new MutateResult.Rejected(rej));
            var rev = Encoding.UTF8.GetBytes(new string(Encoding.UTF8.GetString(op).Reverse().ToArray()));
            return new ValueTask<MutateResult>(new MutateResult.Applied(rev));
        }));
        using var res = NativeFencedResource.Foreign(up);
        byte[] abc = Encoding.UTF8.GetBytes("abc");
        var ok = await Within(res.MutateAsync(new Fence("tree/42", 7, "alice"), "alice", abc));
        Assert.Equal(Encoding.UTF8.GetBytes("cba"), Assert.IsType<MutateResult.Applied>(ok).Output);
        Assert.Equal(new MutateResult.Rejected(FenceRejection.StaleEpoch(6, 7)), await Within(res.MutateAsync(new Fence("tree/42", 6, "alice"), "alice", abc)));
        Assert.Equal(new MutateResult.Rejected(FenceRejection.FutureEpoch(9, 7)), await Within(res.MutateAsync(new Fence("tree/42", 9, "alice"), "alice", abc)));
        Assert.Equal(new MutateResult.Rejected(FenceRejection.NotHolder()), await Within(res.MutateAsync(new Fence("tree/42", 7, "alice"), "mallory", abc)));
    }

    [Fact]
    public async Task Fenced_Mutation_Without_An_Answer_Is_An_Unknown_Outcome_Never_Failed_Open()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        up.ServeFencedResource(new FuncFenced(async (_, _, _, ct) =>
        {
            await Task.Delay(TimeSpan.FromSeconds(10), ct);
            return new MutateResult.Applied(Array.Empty<byte>());
        }));
        // Even a fail-open policy must not turn a missing answer into "applied".
        up.SetPolicy(UpcallKind.FencedMutate, TimeSpan.FromMilliseconds(150), HookFailureMode.FailOpen);
        using var res = NativeFencedResource.Foreign(up);
        await Assert.ThrowsAsync<IrohException>(() => Within(res.MutateAsync(new Fence("r", 1, "a"), "a", new byte[] { 1 })));
    }

    [Fact]
    public async Task Admitted_Filter_Is_Batched_And_Cached()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        var batches = new ConcurrentQueue<IReadOnlyList<string>>();
        up.ServeAdmittedFilter(new FuncFilter((ids, _) =>
        {
            batches.Enqueue(ids);
            return new ValueTask<IReadOnlyList<bool>>(ids.Select(i => i.StartsWith('a')).ToList());
        }));
        string a = new('a', 64), b = new('b', 64), c = new('c', 64);
        using var f = AdmittedFilterHandle.Create(up);

        await Within(f.PrefetchAsync([a, b]));
        Assert.Single(batches);
        Assert.Equal(new[] { a, b }, batches.First());
        Assert.True(f.IsAdmitted(a));
        Assert.False(f.IsAdmitted(b));
        Assert.False(f.IsAdmitted(c)); // miss: fail-closed answer, resolved in the background
        await Eventually(() => batches.Count >= 2, "background resolution");
        f.Set(b, true);
        Assert.True(f.IsAdmitted(b));
        f.Clear();
        var s = f.Stats();
        Assert.Equal(3ul, s.Hits);
        Assert.Equal(1ul, s.Misses);
        Assert.True(s.Upcalls >= 2);
        Assert.ThrowsAny<IrohException>(() => f.IsAdmitted("zz"));
    }

    [Fact]
    public async Task Enable_Shared_Installs_The_Filter()
    {
        await using var node = await Node.StartAsync(new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0", EnableMonitoring = true });
        await using var up = Upcalls.Create(node.Runtime);
        up.ServeAdmittedFilter(new FuncFilter((ids, _) => new ValueTask<IReadOnlyList<bool>>(ids.Select(_ => true).ToList())));
        using var f = AdmittedFilterHandle.Create(up);
        var secret = Enumerable.Repeat((byte)0x42, 32).ToArray();
        await Within(Topology.EnableSharedAsync(node, secret, f, [], TimeSpan.FromSeconds(1)));
        _ = f.Stats();
        Assert.Throws<ArgumentException>(() => Topology.EnableSharedAsync(node, new byte[5]).GetAwaiter().GetResult());
    }

    [Fact]
    public async Task Disposing_The_Table_Fails_Pending_Upcalls_And_Cancels_Handlers()
    {
        using var rt = new Runtime();
        var up = Upcalls.Create(rt);
        var entered = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var cancelledWith = new TaskCompletionSource<UpcallCancelReason>(TaskCreationOptions.RunContinuationsAsynchronously);
        up.Handle(UpcallKind.LeaseTransition, async (req, ctx) =>
        {
            entered.TrySetResult();
            try { await Task.Delay(TimeSpan.FromSeconds(30), ctx.Cancellation); }
            catch (OperationCanceledException) { cancelledWith.TrySetResult(ctx.CancelReason); throw; }
            return null;
        });
        using var ser = NativeLeaseSerializer.Foreign(up);
        var pending = ser.AcquireAsync("r", "a");
        await Within(entered.Task);
        await up.DisposeAsync();
        await Assert.ThrowsAsync<IrohException>(() => Within(pending));
        Assert.Equal(UpcallCancelReason.Shutdown, await Within(cancelledWith.Task));
        Assert.Equal(0, up.ActiveHandlers);
        // Idempotent, and nothing is reachable afterwards.
        await up.DisposeAsync();
        up.Dispose();
        Assert.False(up.Respond(1, UpcallStatus.Ok, ReadOnlySpan<byte>.Empty));
        Assert.Throws<ObjectDisposedException>(() => up.SetDefaultPolicy(TimeSpan.FromSeconds(1), HookFailureMode.FailClosed));
    }

    [Fact]
    public async Task No_Handler_Starts_After_Dispose_Begins()
    {
        using var rt = new Runtime();
        var up = Upcalls.Create(rt);
        int started = 0;
        up.Handle(UpcallKind.LeaseTransition, async (req, ctx) =>
        {
            Interlocked.Increment(ref started);
            await Task.Delay(20);
            return Ucv1.EncodeTransitionOutcome(new TransitionOutcome.Renewed());
        });
        using var ser = NativeLeaseSerializer.Foreign(up);
        // Fire a burst of acquires, dispose while they are in flight; none may fail the process or leak a handler.
        var calls = Enumerable.Range(0, 50).Select(i => ser.AcquireAsync("r" + i, "a")).ToList();
        await Task.Delay(30);
        await up.DisposeAsync();
        int atDispose = Volatile.Read(ref started);
        await Task.Delay(100);
        Assert.Equal(atDispose, Volatile.Read(ref started));
        Assert.Equal(0, up.ActiveHandlers);
        foreach (var c in calls)
        {
            try { await Within(c); } catch (IrohException) { /* failed by the close, or the OK answer was not a grant */ }
        }
    }

    [Fact]
    public async Task Concurrent_Upcalls_Do_Not_Starve_The_Thread_Pool()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt, 4096);
        long inFlight = 0, peak = 0;
        up.ServeFencedResource(new FuncFenced(async (fence, _, op, ct) =>
        {
            long n = Interlocked.Increment(ref inFlight);
            long p;
            while (n > (p = Interlocked.Read(ref peak))) Interlocked.CompareExchange(ref peak, n, p);
            await Task.Delay(100, ct);
            Interlocked.Decrement(ref inFlight);
            return new MutateResult.Applied(op);
        }));
        using var res = NativeFencedResource.Foreign(up);
        var calls = Enumerable.Range(0, 300).Select(i => res.MutateAsync(new Fence("r", 1, "a"), "a", BitConverter.GetBytes(i))).ToList();
        // The pool must stay responsive while hundreds of upcalls are parked.
        var probe = Task.Run(() => 1);
        Assert.Equal(1, await probe.WaitAsync(TimeSpan.FromSeconds(2)));
        var all = await Task.WhenAll(calls).WaitAsync(T);
        for (int i = 0; i < all.Length; i++)
            Assert.Equal(BitConverter.GetBytes(i), Assert.IsType<MutateResult.Applied>(all[i]).Output);
        Assert.True(Interlocked.Read(ref peak) > 50, $"handlers did not overlap (peak {peak})");
    }

    [Fact]
    public async Task A_Full_Queue_Refuses_At_Once()
    {
        using var rt = new Runtime();
        // Capacity 2, a handler that never answers: the queue fills; the rest are refused (Overloaded) not hung.
        await using var up = Upcalls.Create(rt, 2);
        var gate = new TaskCompletionSource();
        up.ServeFencedResource(new FuncFenced(async (_, _, _, ct) =>
        {
            await gate.Task.WaitAsync(ct);
            return new MutateResult.Applied(Array.Empty<byte>());
        }));
        up.SetPolicy(UpcallKind.FencedMutate, TimeSpan.FromSeconds(5), HookFailureMode.FailClosed);
        using var res = NativeFencedResource.Foreign(up);
        var calls = Enumerable.Range(0, 40).Select(i => res.MutateAsync(new Fence("r", 1, "a"), "a", new byte[] { 1 })).ToList();
        // Whatever the poll loop already took off the queue is running; the others are refused immediately or answered later.
        await Task.Delay(300);
        gate.SetResult();
        int ok = 0, refused = 0;
        foreach (var c in calls)
        {
            try { await Within(c); ok++; } catch (IrohException) { refused++; }
        }
        Assert.Equal(40, ok + refused);
        Assert.True(ok >= 1);
    }

    // Pointers and `&` are not usable in an async method, so the synchronous native driving lives here.
    private static unsafe ulong DriveAuthorizeRequest(Runtime rt, ulong table, ulong ser, NativeScope scope)
    {
        Native.Check(Native.aster_lease_acquire(rt.Handle, ser, scope.Utf8("db"), scope.Utf8("alice"), 0, 0, 0, 0, out ulong op), "acquire");

        UpcallEventNative ev = default;
        uint n = 0;
        var end = DateTime.UtcNow + T;
        while (n == 0 && DateTime.UtcNow < end) n = Native.aster_upcall_poll(rt.Handle, table, &ev, 1, 100);
        Assert.Equal(1u, n);
        Assert.Equal((ushort)UpcallKind.LeaseAuthorize, ev.kind);
        Assert.Equal(0, ev.cancel_reason);
        Assert.Equal(Ucv1.TagLeaseAuthorizeRequest, *(byte*)ev.payload_ptr);
        Assert.NotEqual(0ul, ev.buffer);

        Assert.Equal(0, Native.aster_upcall_buffer_release(rt.Handle, table, ev.buffer));
        // A second release of the same buffer is refused, never a double free.
        Assert.Equal((int)Status.NotFound, Native.aster_upcall_buffer_release(rt.Handle, table, ev.buffer));
        // 0 is a no-op.
        Assert.Equal(0, Native.aster_upcall_buffer_release(rt.Handle, table, 0));

        byte[] allow = Ucv1.EncodeDecision(true);
        fixed (byte* p = allow)
        {
            Assert.Equal(0, Native.aster_upcall_respond(rt.Handle, table, ev.invocation_id, (uint)UpcallStatus.Ok, p, (uint)allow.Length));
            Assert.Equal((int)Status.NotFound, Native.aster_upcall_respond(rt.Handle, table, ev.invocation_id, (uint)UpcallStatus.Ok, p, (uint)allow.Length));
        }
        return op;
    }

    [Fact]
    public async Task Raw_Poll_Releases_Each_Buffer_Exactly_Once_And_Respond_Is_Idempotent()
    {
        using var rt = new Runtime();
        // Drive the C ABI directly (no managed poll loop): memory serializer + authorizer upcalls.
        Native.Check(Native.aster_upcalls_new(rt.Handle, 8, out ulong table), "upcalls_new");
        Native.Check(Native.aster_lease_serializer_memory_new(rt.Handle, table, out ulong ser), "memory_new");
        try
        {
            using var scope = new NativeScope();
            ulong op = DriveAuthorizeRequest(rt, table, ser, scope);
            var done = await rt.WaitForAsync(op).WaitAsync(T);
            Assert.Equal((uint)EventKind.LeaseAcquired, done.kind);
            ulong lease = done.handle;
            EventError.TakePayload(rt, in done);
            Assert.NotEqual(0ul, lease);
            Native.aster_lease_free(rt.Handle, lease);
        }
        finally
        {
            Native.aster_lease_serializer_free(rt.Handle, ser);
            Native.aster_upcalls_free(rt.Handle, table);
        }
    }

    [Fact]
    public async Task Cancel_Events_Are_Released_And_Counted_Under_Repeated_Timeouts()
    {
        using var rt = new Runtime();
        await using var up = Upcalls.Create(rt);
        int observed = 0;
        up.Handle(UpcallKind.LeaseAuthorize, async (req, ctx) =>
        {
            try { await Task.Delay(TimeSpan.FromSeconds(10), ctx.Cancellation); }
            catch (OperationCanceledException) { Interlocked.Increment(ref observed); throw; }
            return Ucv1.EncodeDecision(true);
        });
        up.SetPolicy(UpcallKind.LeaseAuthorize, TimeSpan.FromMilliseconds(50), HookFailureMode.FailClosed);
        using var ser = NativeLeaseSerializer.Memory(rt, up);
        for (int i = 0; i < 25; i++)
            Assert.IsType<AcquireResult.Denied>(await Within(ser.AcquireAsync("r" + i, "x")));
        await Eventually(() => Volatile.Read(ref observed) == 25, "all 25 handlers cancelled");
        Assert.Equal(25, up.CancelCount);
        await Eventually(() => up.ActiveHandlers == 0, "handlers drained");
    }
}
