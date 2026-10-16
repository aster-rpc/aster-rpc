using System;
using System.Collections.Generic;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Aster.Interceptors;

namespace Aster;

/// <summary>Overrides <c>[identity]</c> fields through NodeConfig; only the fields set apply. <see cref="TrustedIssuers"/> replaces the whole list.</summary>
public sealed record IdentityOverride
{
    public bool? AcceptOnBehalfOf { get; init; }
    public IReadOnlyList<TrustedIssuer>? TrustedIssuers { get; init; }
    public ulong? EdgeTokenMaxLifetimeSecs { get; init; }
    public ulong? ClockLeewaySecs { get; init; }
    public ulong? MaxKeySetAgeSecs { get; init; }
}

/// <summary>
/// Replaces a node's whole NodeConfig record (Rust <c>NodeConfigSetRequest</c>). <see cref="ExpectedGeneration"/>
/// 0 is unconditional; otherwise the set is <c>ABORTED</c> unless it matches.
/// </summary>
public sealed record NodeConfigSetRequest
{
    public ulong ExpectedGeneration { get; init; }
    public string Name { get; init; } = "";
    public IReadOnlyList<string> Tags { get; init; } = Array.Empty<string>();
    /// <summary>Typed attributes; keys may not start with <c>aster.</c>.</summary>
    public IReadOnlyDictionary<string, NodeAttribute> Attributes { get; init; } = new Dictionary<string, NodeAttribute>();
    /// <summary>Overrides <c>[identity]</c> (null: not overridden).</summary>
    public IdentityOverride? Identity { get; init; }
    /// <summary>Other section overrides by name, as JSON objects.</summary>
    public IReadOnlyDictionary<string, JsonElement>? Sections { get; init; }
}

/// <summary>One overridable setting's value in force and where it came from (<c>default</c>, <c>file</c>, <c>env</c>, <c>code</c> or <c>override</c>).</summary>
public sealed record EffectiveSetting
{
    public string Key { get; init; } = "";
    public JsonElement Value { get; init; }
    public string Source { get; init; } = "";
}

/// <summary>NodeConfig <c>get</c>: the record and every overridable setting's effective value.</summary>
public sealed record NodeConfigView
{
    public string NodeId { get; init; } = "";
    public long Generation { get; init; }
    public long AppliedGeneration { get; init; }
    public string Change { get; init; } = "";
    public string ChangedBy { get; init; } = "";
    public long ChangedAtMs { get; init; }
    public string Name { get; init; } = "";
    public IReadOnlyList<string> Tags { get; init; } = Array.Empty<string>();
    public IReadOnlyDictionary<string, NodeAttribute> Attributes { get; init; } = new Dictionary<string, NodeAttribute>();
    public IReadOnlyDictionary<string, JsonElement> Sections { get; init; } = new Dictionary<string, JsonElement>();
    public IReadOnlyList<string> IgnoredSections { get; init; } = Array.Empty<string>();
    public IReadOnlyList<EffectiveSetting> Settings { get; init; } = Array.Empty<EffectiveSetting>();
    public IReadOnlyList<string> OverridableSections { get; init; } = Array.Empty<string>();
    public IReadOnlyList<string> LockedSections { get; init; } = Array.Empty<string>();

    /// <summary>One setting by <c>section.field</c> key, or null.</summary>
    public EffectiveSetting? Setting(string key)
    {
        foreach (var s in Settings)
            if (s.Key == key) return s;
        return null;
    }
}

/// <summary>A part of a record not (yet) in effect.</summary>
public sealed record NodeConfigOutstanding
{
    public string Part { get; init; } = "";
    public string State { get; init; } = "";
    public string Detail { get; init; } = "";
}

/// <summary>The answer to set, clear and ack: <see cref="Complete"/> only once the generation asked about is stored and in effect.</summary>
public sealed record NodeConfigAck
{
    public bool Complete { get; init; }
    public long Generation { get; init; }
    public long AppliedGeneration { get; init; }
    public IReadOnlyList<NodeConfigOutstanding> Outstanding { get; init; } = Array.Empty<NodeConfigOutstanding>();
}

/// <summary>
/// Calls <c>aster.ops.NodeConfig</c> on the server at the other end of an <see cref="RpcConnection"/> (Rust
/// <c>NodeConfigClient</c>; C ABI 7.7 <c>aster_node_config_call</c>). Every method needs
/// <see cref="GatewayIdentity.ConfigAdminRole"/> from this node's own Enrollment credential (Gate 3); a refusal
/// is an <see cref="RpcError"/> with the server's status.
/// </summary>
public sealed class NodeConfigClient
{
    private readonly RpcConnection _rc;

    public NodeConfigClient(RpcConnection connection)
    {
        ArgumentNullException.ThrowIfNull(connection);
        _rc = connection;
    }

    /// <summary>The record and every overridable setting's effective value.</summary>
    public Task<NodeConfigView> GetAsync(TimeSpan? deadline = null, CancellationToken cancellationToken = default)
        => CallAsync<NodeConfigView>(Native.NodeConfigGet, new Dictionary<string, object?>(), deadline, cancellationToken);

    /// <summary>Replaces the record; answers once the new generation is in effect.</summary>
    public Task<NodeConfigAck> SetAsync(NodeConfigSetRequest request, TimeSpan? deadline = null, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(request);
        var sections = new Dictionary<string, object?>();
        if (request.Sections is { } extra)
            foreach (var (k, v) in extra) sections[k] = v;
        if (request.Identity is { } identity)
            sections["identity"] = identity;
        return CallAsync<NodeConfigAck>(Native.NodeConfigSet, new Dictionary<string, object?>
        {
            ["expected_generation"] = request.ExpectedGeneration,
            ["name"] = request.Name,
            ["tags"] = request.Tags,
            ["attributes"] = request.Attributes,
            ["sections"] = sections,
        }, deadline, cancellationToken);
    }

    /// <summary>Reverts the node to its file configuration (a tombstone with the next generation); answers once that is in effect.</summary>
    public Task<NodeConfigAck> ClearAsync(ulong expectedGeneration = 0, TimeSpan? deadline = null, CancellationToken cancellationToken = default)
        => CallAsync<NodeConfigAck>(Native.NodeConfigClear, new Dictionary<string, object?> { ["expected_generation"] = expectedGeneration },
            deadline, cancellationToken);

    /// <summary>Whether <paramref name="generation"/> is stored and in effect, waiting up to <paramref name="timeout"/> (null = 10 s, at most 60 s).</summary>
    public Task<NodeConfigAck> AckAsync(ulong generation, TimeSpan? timeout = null, TimeSpan? deadline = null, CancellationToken cancellationToken = default)
        => CallAsync<NodeConfigAck>(Native.NodeConfigAck, new Dictionary<string, object?>
        {
            ["generation"] = generation,
            ["timeout_ms"] = timeout is { } t ? (ulong)Math.Max(0, Math.Ceiling(t.TotalMilliseconds)) : 0UL,
        }, deadline, cancellationToken);

    private async Task<T> CallAsync<T>(uint method, Dictionary<string, object?> request, TimeSpan? deadline, CancellationToken ct)
    {
        if (deadline is { } d)
        {
            if (d <= TimeSpan.Zero) throw new RpcError(StatusCode.DeadlineExceeded, "deadline exceeded before the call");
            request["deadline_ms"] = (ulong)Math.Ceiling(d.TotalMilliseconds);
        }
        ct.ThrowIfCancellationRequested();
        byte[] body = JsonSerializer.SerializeToUtf8Bytes(request, GatewayIdentity.Json);
        var conn = _rc.Connection;
        var runtime = conn.Runtime;
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_node_config_call(runtime.Handle, conn.Handle.Value, method, scope.Bytes(body), 0, out op),
                "aster_node_config_call");
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
        if (ev.kind != (uint)EventKind.NodeConfigResult)
            throw EventError.From(runtime, ev, "aster.ops.NodeConfig");
        byte[] payload = EventError.TakePayload(runtime, in ev);
        ResultJson res;
        try { res = JsonSerializer.Deserialize<ResultJson>(payload, GatewayIdentity.Json) ?? new ResultJson(); }
        catch (JsonException e) { throw new IrohException(Status.Internal, $"aster.ops.NodeConfig: decode result: {e.Message}"); }
        if (res.Code != 0)
            throw new RpcError((StatusCode)res.Code, res.Message ?? "");
        try { return res.Result.Deserialize<T>(GatewayIdentity.Json) ?? throw new IrohException(Status.Internal, "aster.ops.NodeConfig: empty result"); }
        catch (JsonException e) { throw new IrohException(Status.Internal, $"aster.ops.NodeConfig: decode result: {e.Message}"); }
    }

    private sealed class ResultJson
    {
        public int Code { get; set; }
        public string? Message { get; set; }
        public JsonElement Result { get; set; }
    }
}
