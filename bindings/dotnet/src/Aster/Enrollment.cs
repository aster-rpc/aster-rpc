using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace Aster;

/// <summary>
/// The Enrollment an <see cref="AsterServer"/> or <see cref="AsterClient"/> installed from its
/// configuration (C ABI 5.1 <c>aster_enrollment_t</c>; Rust <c>aster::enrollment::Enrollment</c>;
/// design <c>docs/_internal/working_ideas/aster-enrollment.md</c> §15). Nothing here is needed for
/// roles or revocation, which apply automatically on every call; these are escape hatches for
/// audit, diagnostics and checks before a sensitive operation. The node owns it: disposing the
/// server or client releases it. Root operations (namespace create, promote, revoke, delegate,
/// rotate) belong to the <c>aster</c> CLI.
/// </summary>
public sealed class Enrollment : IDisposable
{
    /// <summary>The ALPN of the reciprocal enrollment handshake.</summary>
    public const string Alpn = "aster.enrollment/1";

    internal static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull,
    };

    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _closed;
    // Servers a client has enrolled with (node id).
    private readonly ConcurrentDictionary<string, bool> _enrolled = new(StringComparer.OrdinalIgnoreCase);

    internal Enrollment(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    private ulong Live()
    {
        if (Volatile.Read(ref _closed) != 0)
            throw new ObjectDisposedException(nameof(Enrollment), "enrollment closed");
        return _handle;
    }

    /// <summary>
    /// Releases the enrollment early (disposing the node does it too): pending
    /// <see cref="NextRevocationAsync"/> waits end and a client stops renewing.
    /// </summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _closed, 1) == 0)
            Native.aster_enrollment_free(_runtime.Handle, _handle);
    }

    /// <summary>The node closed: its enrollment handle is gone.</summary>
    internal void MarkClosed() => Interlocked.Exchange(ref _closed, 1);

    /// <summary>This node's Enrollment state.</summary>
    public unsafe EnrollmentStatus Status()
    {
        Bytes output = default;
        Native.Check(Native.aster_enrollment_status(_runtime.Handle, Live(), &output), "aster_enrollment_status");
        return Decode<EnrollmentStatus>(NativeScope.TakeOwned(ref output), "aster_enrollment_status");
    }

    /// <summary>This node's current credential evidence (it follows renewals).</summary>
    public PeerEvidence? Credential() => Status().Credential;

    /// <summary>
    /// The verified evidence the peer's admission session rests on (subject, issuer path, scopes,
    /// tags, attributes, validity), for audit; null when the peer has no session (never enrolled,
    /// revoked, evicted). A server already maps it to roles on every call (<c>[enrollment.roles]</c>);
    /// do not copy it into peer attributes.
    /// </summary>
    public unsafe PeerEvidence? PeerClaims(string peer)
    {
        ArgumentException.ThrowIfNullOrEmpty(peer);
        Bytes output = default;
        int status;
        using (var scope = new NativeScope())
            status = Native.aster_enrollment_peer_claims(_runtime.Handle, Live(), scope.Utf8(peer), &output);
        if (status == (int)Aster.Status.NotFound) return null;
        Native.Check(status, "aster_enrollment_peer_claims");
        return Decode<PeerEvidence>(NativeScope.TakeOwned(ref output), "aster_enrollment_peer_claims");
    }

    /// <summary>
    /// Re-evaluates an admitted peer against current policy (call it before a sensitive
    /// operation). An <see cref="EnrollmentAdmissionException"/> says why not; only
    /// <see cref="AdmissionErrorKind.Denied"/> revokes the session.
    /// </summary>
    public async Task<Readiness> CheckPeerAsync(string peer, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(peer);
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_enrollment_check_peer(_runtime.Handle, Live(), scope.Utf8(peer), 0, out op), "aster_enrollment_check_peer");
        return Decode<Readiness>(await ResultAsync(op, "aster_enrollment_check_peer", cancellationToken).ConfigureAwait(false),
            "aster_enrollment_check_peer");
    }

    /// <summary>
    /// Waits for the next revocation of a formerly admitted peer (buffered from start). Its
    /// connections are already closed and its roles are gone from the next call; use this for
    /// application state keyed by peer. Cancelling consumes nothing.
    /// </summary>
    public async Task<Revocation> NextRevocationAsync(CancellationToken cancellationToken = default)
    {
        Native.Check(Native.aster_enrollment_next_revocation(_runtime.Handle, Live(), 0, out ulong op), "aster_enrollment_next_revocation");
        return Decode<Revocation>(await ResultAsync(op, "aster_enrollment_next_revocation", cancellationToken).ConfigureAwait(false),
            "aster_enrollment_next_revocation");
    }

    /// <summary>
    /// The client sequence against one server, once per server (design §15.7 steps 3, 4 and 6):
    /// the reciprocal handshake, synchronizing the fleet namespace the server offers, and renewal
    /// when the server signed this node's credential (or <see cref="EnrollmentConfig.RenewFrom"/> is set).
    /// </summary>
    internal async Task EnrollAsync(NodeAddrInfo server, bool force, CancellationToken cancellationToken)
    {
        if (!force && _enrolled.ContainsKey(server.EndpointId)) return;
        byte[] body = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["node_id"] = server.EndpointId,
            ["relay_url"] = string.IsNullOrEmpty(server.RelayUrl) ? null : server.RelayUrl,
            ["direct_addresses"] = server.DirectAddresses.ToArray(),
        });
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_enrollment_enroll(_runtime.Handle, Live(), scope.Bytes(body), 0, out op), "aster_enrollment_enroll");
        await ResultAsync(op, "aster_enrollment_enroll", cancellationToken).ConfigureAwait(false);
        _enrolled[server.EndpointId] = true;
    }

    internal void Forget(string serverId) => _enrolled.TryRemove(serverId, out _);

    private async Task<byte[]> ResultAsync(ulong op, string what, CancellationToken ct)
    {
        Event ev;
        try
        {
            ev = await _runtime.WaitForAsync(op, ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            try { _runtime.Cancel(op); } catch (IrohException) { /* already completed */ }
            throw;
        }
        if (ev.kind == (uint)EventKind.EnrollmentResult)
            return EventError.TakePayload(_runtime, in ev);
        var kind = (AdmissionErrorKind)ev.flags;
        var error = EventError.From(_runtime, ev, what);
        if (ev.kind == (uint)EventKind.Error && kind != AdmissionErrorKind.Other && Enum.IsDefined(kind))
            throw new EnrollmentAdmissionException(kind, error.Status, error.Message);
        throw error;
    }

    internal static T Decode<T>(byte[] json, string what)
    {
        try
        {
            return JsonSerializer.Deserialize<T>(json, Json) ?? throw new IrohException(Aster.Status.Internal, $"{what}: empty result");
        }
        catch (JsonException e)
        {
            throw new IrohException(Aster.Status.Internal, $"{what}: decode result: {e.Message}");
        }
    }

    // ── Configured issuer ─────────────────────────────────────────────────

    internal unsafe IssuerInfo IssuerInfo()
    {
        Bytes output = default;
        Native.Check(Native.aster_enrollment_issuer_info(_runtime.Handle, Live(), &output), "aster_enrollment_issuer_info");
        return Decode<IssuerInfo>(NativeScope.TakeOwned(ref output), "aster_enrollment_issuer_info");
    }

    internal unsafe IssuedCredential Issue(string subject, EnrollmentClaims? claims, Registration registration)
    {
        ArgumentException.ThrowIfNullOrEmpty(subject);
        byte[] body = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["subject"] = subject,
            ["claims"] = claims,
            ["registration"] = registration == Registration.Persistent ? "persistent" : "ephemeral",
        }, Json);
        Bytes output = default;
        int status;
        using (var scope = new NativeScope())
            status = Native.aster_enrollment_issue(_runtime.Handle, Live(), scope.Bytes(body), &output);
        if (status == (int)Aster.Status.PermissionDenied && output.ptr != IntPtr.Zero)
        {
            var refusal = Decode<IssuerRefusalJson>(NativeScope.TakeOwned(ref output), "aster_enrollment_issue");
            throw new IssuerRefusalException(refusal.Code, refusal.Message, refusal.Retryable);
        }
        Native.Check(status, "aster_enrollment_issue");
        return Decode<IssuedCredential>(NativeScope.TakeOwned(ref output), "aster_enrollment_issue");
    }

    private sealed class IssuerRefusalJson
    {
        public string Code { get; set; } = "";
        public string Message { get; set; } = "";
        public bool Retryable { get; set; }
    }
}

/// <summary>
/// The delegated issuer a server runs when <see cref="EnrollmentConfig.IssuerDelegationFile"/> is
/// configured (<see cref="AsterServer.Issuer"/>; Rust <c>aster::enrollment::Issuer</c>). It already
/// serves <c>aster.enrollment.IssuerV1</c> renewals; <see cref="Issue"/> enrolls new nodes.
/// </summary>
public sealed class EnrollmentIssuer
{
    private readonly Enrollment _enrollment;

    internal EnrollmentIssuer(Enrollment enrollment) => _enrollment = enrollment;

    /// <summary>The issuer's key and delegation expiry.</summary>
    public IssuerInfo Info() => _enrollment.IssuerInfo();

    /// <summary>
    /// Signs a credential for <paramref name="subject"/> (a node id the orchestrator authenticated
    /// out of band). A refusal is an <see cref="IssuerRefusalException"/>.
    /// </summary>
    public IssuedCredential Issue(string subject, EnrollmentClaims? claims = null, Registration registration = Registration.Ephemeral)
        => _enrollment.Issue(subject, claims, registration);
}

/// <summary>What an issued credential asks for.</summary>
public enum Registration
{
    /// <summary>The credential is the node's whole authority (default).</summary>
    Ephemeral,
    /// <summary>Admits the node until the root promotes it.</summary>
    Persistent,
}

/// <summary>The claims a credential carries. A null set is unknown; an empty one is complete and empty.</summary>
public sealed record EnrollmentClaims
{
    public IReadOnlyList<string>? Scopes { get; init; }
    public IReadOnlyList<string>? Tags { get; init; }
    public IReadOnlyDictionary<string, ClaimAttribute>? Attributes { get; init; }
}

/// <summary>The verified evidence a peer's admission rests on.</summary>
public sealed record PeerEvidence
{
    public string Subject { get; init; } = "";
    /// <summary>The trusted root the evidence is anchored at.</summary>
    public string Root { get; init; } = "";
    /// <summary>The leaf signer (a delegated issuer, or the root); for a registration, the root.</summary>
    public string Issuer { get; init; } = "";
    /// <summary>The chain's issuer path, leaf signer first (empty for a registration).</summary>
    public IReadOnlyList<string> Issuers { get; init; } = Array.Empty<string>();
    /// <summary><c>"chain"</c> or <c>"registration"</c>.</summary>
    public string Source { get; init; } = "";
    public string? ChainDigest { get; init; }
    public string? DocumentId { get; init; }
    public ulong? Revision { get; init; }
    public string? CredentialId { get; init; }
    public IReadOnlyList<string> Audience { get; init; } = Array.Empty<string>();
    /// <summary>Null when unknown.</summary>
    public IReadOnlyList<string>? Scopes { get; init; }
    /// <summary>Null when unknown.</summary>
    public IReadOnlyList<string>? Tags { get; init; }
    /// <summary>Null when unknown.</summary>
    public IReadOnlyDictionary<string, ClaimAttribute>? Attributes { get; init; }
    public ulong IssuedAt { get; init; }
    public ulong? NotBefore { get; init; }
    public ulong? ExpiresAt { get; init; }
    /// <summary>What a presented credential asks for (<c>"ephemeral"</c> / <c>"persistent"</c>); null for a registration record.</summary>
    public string? Registration { get; init; }

    /// <summary>Whether a root-authored registration admits the peer.</summary>
    public bool Registered => Source == "registration";
}

/// <summary>A peer's admission (membership only, never operation permission).</summary>
public sealed record Readiness
{
    public string Peer { get; init; } = "";
    /// <summary><c>"chain"</c> or <c>"registration"</c>.</summary>
    public string Source { get; init; } = "";
    public string? ObservedSource { get; init; }
    public PolicySync? PolicySync { get; init; }
    /// <summary><c>synchronized</c>, <c>unsynchronized</c> or <c>not_checked</c>.</summary>
    public string? LocalClock { get; init; }
}

/// <summary>The last completed policy synchronization.</summary>
public sealed record PolicySync
{
    public ulong CompletedAt { get; init; }
    /// <summary><c>"root"</c> or the replica's node id.</summary>
    public string Source { get; init; } = "";
    public ulong? ObservedRevision { get; init; }
}

/// <summary>Why a peer was denied; definite reasons revoke sessions.</summary>
public sealed record DenialReason
{
    /// <summary>e.g. <c>node_revoked</c>, <c>credential_revoked</c>, <c>issuer_revoked</c>.</summary>
    public string Code { get; init; } = "";
    public bool Definite { get; init; }
    public JsonElement? Detail { get; init; }
}

/// <summary>
/// A formerly admitted peer revoked by a definite denial (its connections are closed and its
/// roles gone from the next call); <see cref="Lagged"/> &gt; 0 instead means that many were missed.
/// </summary>
public sealed record Revocation
{
    public string? Peer { get; init; }
    public DenialReason? Reason { get; init; }
    public ulong Lagged { get; init; }
}

/// <summary>This node's Enrollment state.</summary>
public sealed record EnrollmentStatus
{
    /// <summary><c>"server"</c> or <c>"client"</c>.</summary>
    public string Side { get; init; } = "";
    public string TrustedRoot { get; init; } = "";
    public string FleetNamespace { get; init; } = "";
    public string Audience { get; init; } = "";
    /// <summary>This node's credential evidence; follows renewals.</summary>
    public PeerEvidence? Credential { get; init; }
    public RootState? RootState { get; init; }
    public IReadOnlyList<string> RootRotations { get; init; } = Array.Empty<string>();
    /// <summary>Whether this node renews its credential.</summary>
    public bool Renewing { get; init; }
    /// <summary>The configured delegated issuer's key (null when none).</summary>
    public string? Issuer { get; init; }
}

/// <summary>The root rotation state.</summary>
public sealed record RootState
{
    public string Root { get; init; } = "";
    public string? NextCommitment { get; init; }
    public ulong Seq { get; init; }
}

/// <summary>An issuer's identity.</summary>
public sealed record IssuerInfo
{
    public string PublicKey { get; init; } = "";
    public ulong DelegationExpiresAt { get; init; }
}

/// <summary>A credential an issuer signed; hand <see cref="Chain"/> to the subject (it is useless without the subject's key).</summary>
public sealed record IssuedCredential
{
    public string Chain { get; init; } = "";
    public string? CredentialId { get; init; }
    public ulong? ExpiresAt { get; init; }
}

/// <summary>Classifies an <see cref="EnrollmentAdmissionException"/> (<c>aster_enrollment_error_t</c>).</summary>
public enum AdmissionErrorKind : uint
{
    Other = 0,
    /// <summary>No admission session: the peer must enroll (again).</summary>
    NotAdmitted = 1,
    /// <summary>A definite denial; only this kind revokes.</summary>
    Denied = 2,
    ClockSkew = 3,
    Unavailable = 4,
}

/// <summary>Why an Enrollment operation did not admit a peer.</summary>
public sealed class EnrollmentAdmissionException : IrohException
{
    public AdmissionErrorKind Kind { get; }

    public EnrollmentAdmissionException(AdmissionErrorKind kind, Status status, string message) : base(status, message)
        => Kind = kind;
}

/// <summary>
/// An issuer refusal (codes as <c>aster.enrollment.IssuerV1</c> reports them: <c>subject_denied</c>,
/// <c>registered</c>, <c>policy_unavailable</c>, <c>foreign_credential</c>, <c>issuer_revoked</c>,
/// <c>invalid_credential</c>, <c>invalid_argument</c>).
/// </summary>
public sealed class IssuerRefusalException : IrohException
{
    public string Code { get; }
    public bool Retryable { get; }

    public IssuerRefusalException(string code, string message, bool retryable)
        : base(Aster.Status.PermissionDenied, $"enrollment issuer refused: {code}: {message}")
    {
        Code = code;
        Retryable = retryable;
    }
}
