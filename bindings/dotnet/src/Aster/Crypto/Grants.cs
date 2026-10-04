using System;

namespace Aster.Crypto;

/// <summary>Local access to a namespace (<c>aster_local_access_t</c>).</summary>
public enum LocalAccess : uint
{
    Write = 1,
    Read = 2,
}

/// <summary>
/// What a sealed grant binds to (<c>aster_grant_context_t</c>): granter and recipient are node
/// id hex strings, bound verbatim.
/// </summary>
public sealed record GrantContext(string App, string Granter, string Recipient, byte[] Resource, string Path, string Role)
{
    /// <summary>The AEAD associated data for this context (see <see cref="Grants.Aad"/>).</summary>
    public byte[] Aad() => Grants.Aad(this);
}

/// <summary>A namespace capability: a write secret or a read-only namespace id.</summary>
public sealed record NamespaceCapability(LocalAccess Access, byte[] Key)
{
    public static NamespaceCapability Read(byte[] namespaceId) => new(LocalAccess.Read, namespaceId);
    public static NamespaceCapability Write(byte[] namespaceSecret) => new(LocalAccess.Write, namespaceSecret);

    public bool CanWrite => Access == LocalAccess.Write;

    /// <summary>The namespace id (derived from the secret for a write capability).</summary>
    public byte[] NamespaceId() => CanWrite ? Ids.NamespaceId(Key) : Key;

    public unsafe byte[] Encode()
    {
        Ids.Require32(Key, nameof(Key));
        Bytes output = default;
        fixed (byte* k = Key)
            Native.Check(Native.aster_namespace_capability_encode((uint)Access, k, &output), "aster_namespace_capability_encode");
        return NativeScope.TakeOwned(ref output);
    }

    public static unsafe NamespaceCapability Decode(ReadOnlySpan<byte> bytes)
    {
        using var scope = new NativeScope();
        uint access;
        var key = new byte[Ids.KeySize];
        fixed (byte* k = key)
            Native.Check(Native.aster_namespace_capability_decode(scope.Bytes(bytes), &access, k), "aster_namespace_capability_decode");
        return new NamespaceCapability((LocalAccess)access, key);
    }

    public bool Equals(NamespaceCapability? other) => other is not null && Access == other.Access && Key.AsSpan().SequenceEqual(other.Key);

    public override int GetHashCode() => HashCode.Combine(Access, Convert.ToHexString(Key));
}

/// <summary>Grant record state (<c>aster_grant_state_t</c>).</summary>
public enum GrantState : uint
{
    Active = 0,
    Revoked = 1,
}

/// <summary>
/// The stored form of a grant (<c>aster_grant_record_t</c>): state, role, the context version
/// it was sealed under, the sealed envelope (active) or the reason (revoked).
/// </summary>
public sealed record GrantRecord(GrantState State, string Role, int ContextVersion, byte[] Envelope, string Reason)
{
    public static GrantRecord Active(string role, int contextVersion, byte[] envelope) => new(GrantState.Active, role, contextVersion, envelope, "");
    public static GrantRecord Revoked(string role, string reason) => new(GrantState.Revoked, role, 0, [], reason);

    public bool IsActive => State == GrantState.Active;

    public unsafe byte[] Encode()
    {
        using var scope = new NativeScope();
        var n = new GrantRecordNative
        {
            struct_size = (uint)sizeof(GrantRecordNative),
            state = (uint)State,
            context_version = ContextVersion,
            role = scope.Utf8(Role),
            envelope = scope.Bytes(Envelope),
            reason = scope.Utf8(Reason),
        };
        Bytes output = default;
        Native.Check(Native.aster_grant_record_encode(&n, &output), "aster_grant_record_encode");
        return NativeScope.TakeOwned(ref output);
    }

    public static unsafe GrantRecord Decode(ReadOnlySpan<byte> bytes)
    {
        using var scope = new NativeScope();
        var n = new GrantRecordNative { struct_size = (uint)sizeof(GrantRecordNative) };
        Native.Check(Native.aster_grant_record_decode(scope.Bytes(bytes), &n), "aster_grant_record_decode");
        try
        {
            return new GrantRecord((GrantState)n.state, n.role.ToUtf8String(), n.context_version, n.envelope.ToArray(), n.reason.ToUtf8String());
        }
        finally { Native.aster_grant_record_release(&n); }
    }
}

/// <summary>Outcome kinds of <see cref="Grants.Reconcile"/> (<c>aster_grant_outcome_kind_t</c>).</summary>
public enum GrantOutcomeKind : uint
{
    Active = 0,
    Revoked = 1,
    Unopenable = 2,
    Absent = 3,
}

/// <summary>The result of reconciling a stored grant record against a context.</summary>
/// <typeparam name="T">The opened payload: <c>byte[]</c> or <see cref="NamespaceCapability"/>.</typeparam>
public abstract record GrantOutcome<T>
{
    private GrantOutcome() { }

    public abstract GrantOutcomeKind Kind { get; }

    /// <summary>The grant opened: <paramref name="Value"/> is the payload.</summary>
    public sealed record Active(T Value, bool DropsCapability) : GrantOutcome<T> { public override GrantOutcomeKind Kind => GrantOutcomeKind.Active; }

    /// <summary>The grant was revoked (checked before any role check).</summary>
    public sealed record Revoked(string Reason, string Role) : GrantOutcome<T> { public override GrantOutcomeKind Kind => GrantOutcomeKind.Revoked; }

    /// <summary>The record exists but cannot be opened (undecodable, role mismatch, wrong recipient).</summary>
    public sealed record Unopenable(string Reason) : GrantOutcome<T> { public override GrantOutcomeKind Kind => GrantOutcomeKind.Unopenable; }

    /// <summary>No record.</summary>
    public sealed record Absent : GrantOutcome<T> { public override GrantOutcomeKind Kind => GrantOutcomeKind.Absent; }
}

/// <summary>
/// Sealed grants (<c>aster/sealed-grant/v1</c>, C ABI 3.4): HPKE envelopes bound to a
/// <see cref="GrantContext"/>, namespace-capability grants, grant records and reconciliation.
/// Any mismatch when opening throws <see cref="IrohException"/> with
/// <see cref="Status.VerificationFailed"/>.
/// </summary>
public static unsafe class Grants
{
    /// <summary>
    /// AAD = for each of <c>["aster/sealed-grant/v1", app, granter, recipient, resource, path,
    /// role]</c>: u32 BE length, then the bytes.
    /// </summary>
    public static byte[] Aad(GrantContext ctx)
    {
        using var scope = new NativeScope();
        var c = Native(scope, ctx);
        Bytes output = default;
        Aster.Native.Check(Aster.Native.aster_grant_context_aad(c, &output), "aster_grant_context_aad");
        return NativeScope.TakeOwned(ref output);
    }

    /// <summary><c>&lt;prefix minus trailing '/'&gt;/grants/&lt;recipient&gt;</c>.</summary>
    public static string GrantKey(string prefix, string recipientNodeId)
    {
        using var scope = new NativeScope();
        Bytes output = default;
        Aster.Native.Check(Aster.Native.aster_grant_key(scope.Utf8(prefix), scope.Utf8(recipientNodeId), &output), "aster_grant_key");
        return System.Text.Encoding.UTF8.GetString(NativeScope.TakeOwned(ref output));
    }

    /// <summary>Whether <paramref name="role"/> allows a capability that can (or cannot) write.</summary>
    public static bool RoleMatches(string role, bool canWrite)
    {
        using var scope = new NativeScope();
        int r = Aster.Native.aster_grant_check_role(scope.Utf8(role), canWrite ? 1u : 0u);
        if (r == (int)Status.VerificationFailed) return false;
        Aster.Native.Check(r, "aster_grant_check_role");
        return true;
    }

    /// <summary>Throws <see cref="IrohException"/> (VerificationFailed) unless <see cref="RoleMatches"/>.</summary>
    public static void CheckRole(string role, bool canWrite)
    {
        using var scope = new NativeScope();
        Aster.Native.Check(Aster.Native.aster_grant_check_role(scope.Utf8(role), canWrite ? 1u : 0u), "aster_grant_check_role");
    }

    /// <summary>Seals to the recipient's identity-derived X25519 key.</summary>
    public static byte[] Seal(GrantContext ctx, ReadOnlySpan<byte> plaintext)
    {
        using var scope = new NativeScope();
        Bytes output = default;
        Aster.Native.Check(Aster.Native.aster_grant_seal(Native(scope, ctx), scope.Bytes(plaintext), &output), "aster_grant_seal");
        return NativeScope.TakeOwned(ref output);
    }

    /// <summary>Seals to an explicit X25519 public key.</summary>
    public static byte[] SealToKey(ReadOnlySpan<byte> recipientPublicKey, GrantContext ctx, ReadOnlySpan<byte> plaintext)
    {
        Ids.Require32(recipientPublicKey, nameof(recipientPublicKey));
        using var scope = new NativeScope();
        Bytes output = default;
        fixed (byte* k = recipientPublicKey)
            Aster.Native.Check(Aster.Native.aster_grant_seal_to_key(k, Native(scope, ctx), scope.Bytes(plaintext), &output), "aster_grant_seal_to_key");
        return NativeScope.TakeOwned(ref output);
    }

    /// <summary>Opens with the recipient's Ed25519 identity secret.</summary>
    public static byte[] Open(ReadOnlySpan<byte> identitySecret, GrantContext ctx, ReadOnlySpan<byte> envelope)
        => OpenCore(identitySecret, ctx, envelope, withKey: false);

    /// <summary>Opens with an explicit X25519 private key.</summary>
    public static byte[] OpenWithKey(ReadOnlySpan<byte> recipientPrivateKey, GrantContext ctx, ReadOnlySpan<byte> envelope)
        => OpenCore(recipientPrivateKey, ctx, envelope, withKey: true);

    private static byte[] OpenCore(ReadOnlySpan<byte> key, GrantContext ctx, ReadOnlySpan<byte> envelope, bool withKey)
    {
        Ids.Require32(key, nameof(key));
        using var scope = new NativeScope();
        Bytes output = default;
        var c = Native(scope, ctx);
        fixed (byte* k = key)
        {
            int r = withKey
                ? Aster.Native.aster_grant_open_with_key(k, c, scope.Bytes(envelope), &output)
                : Aster.Native.aster_grant_open(k, c, scope.Bytes(envelope), &output);
            Aster.Native.Check(r, withKey ? "aster_grant_open_with_key" : "aster_grant_open");
        }
        return NativeScope.TakeOwned(ref output);
    }

    /// <summary>Seals a namespace capability (the role rule is checked).</summary>
    public static byte[] SealNamespace(GrantContext ctx, NamespaceCapability capability)
    {
        Ids.Require32(capability.Key, nameof(capability));
        using var scope = new NativeScope();
        Bytes output = default;
        fixed (byte* k = capability.Key)
            Aster.Native.Check(Aster.Native.aster_namespace_grant_seal(Native(scope, ctx), (uint)capability.Access, k, &output), "aster_namespace_grant_seal");
        return NativeScope.TakeOwned(ref output);
    }

    /// <summary>Opens a namespace grant with the recipient's identity secret (the role rule is checked).</summary>
    public static NamespaceCapability OpenNamespace(ReadOnlySpan<byte> identitySecret, GrantContext ctx, ReadOnlySpan<byte> envelope)
    {
        Ids.Require32(identitySecret, nameof(identitySecret));
        using var scope = new NativeScope();
        uint access;
        var key = new byte[Ids.KeySize];
        fixed (byte* s = identitySecret)
        fixed (byte* k = key)
            Aster.Native.Check(Aster.Native.aster_namespace_grant_open(s, Native(scope, ctx), scope.Bytes(envelope), &access, k), "aster_namespace_grant_open");
        return new NamespaceCapability((LocalAccess)access, key);
    }

    /// <summary>Reconciles a stored record (null = none) carrying a raw payload.</summary>
    public static GrantOutcome<byte[]> Reconcile(ReadOnlySpan<byte> identitySecret, GrantContext ctx, byte[]? record)
        => ReconcileCore(identitySecret, ctx, record, ns: false, (n) => n.payload.ToArray());

    /// <summary>Reconciles a stored record (null = none) carrying a namespace capability.</summary>
    public static GrantOutcome<NamespaceCapability> ReconcileNamespace(ReadOnlySpan<byte> identitySecret, GrantContext ctx, byte[]? record)
        => ReconcileCore(identitySecret, ctx, record, ns: true, (n) => new NamespaceCapability((LocalAccess)n.access, n.payload.ToArray()));

    private static GrantOutcome<T> ReconcileCore<T>(ReadOnlySpan<byte> identitySecret, GrantContext ctx, byte[]? record, bool ns,
        Func<GrantOutcomeNative, T> active)
    {
        Ids.Require32(identitySecret, nameof(identitySecret));
        using var scope = new NativeScope();
        var c = Native(scope, ctx);
        var o = new GrantOutcomeNative { struct_size = (uint)sizeof(GrantOutcomeNative) };
        Bytes rec = record is null ? default : scope.Bytes(record);
        fixed (byte* s = identitySecret)
        {
            int r = ns
                ? Aster.Native.aster_namespace_grant_reconcile(s, c, record is null ? null : &rec, &o)
                : Aster.Native.aster_grant_reconcile(s, c, record is null ? null : &rec, &o);
            Aster.Native.Check(r, ns ? "aster_namespace_grant_reconcile" : "aster_grant_reconcile");
        }
        try
        {
            return (GrantOutcomeKind)o.kind switch
            {
                GrantOutcomeKind.Active => new GrantOutcome<T>.Active(active(o), o.drops_capability != 0),
                GrantOutcomeKind.Revoked => new GrantOutcome<T>.Revoked(o.reason.ToUtf8String(), o.role.ToUtf8String()),
                GrantOutcomeKind.Unopenable => new GrantOutcome<T>.Unopenable(o.reason.ToUtf8String()),
                _ => new GrantOutcome<T>.Absent(),
            };
        }
        finally { Aster.Native.aster_grant_outcome_release(&o); }
    }

    private static GrantContextNative* Native(NativeScope scope, GrantContext ctx)
    {
        var c = scope.Struct<GrantContextNative>();
        c->struct_size = (uint)sizeof(GrantContextNative);
        c->app = scope.Utf8(ctx.App);
        c->granter = scope.Utf8(ctx.Granter);
        c->recipient = scope.Utf8(ctx.Recipient);
        c->resource = scope.Bytes(ctx.Resource);
        c->path = scope.Utf8(ctx.Path);
        c->role = scope.Utf8(ctx.Role);
        return c;
    }
}
