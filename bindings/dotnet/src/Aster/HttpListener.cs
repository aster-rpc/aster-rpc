using System.Runtime.InteropServices;
using System.Text;
using Aster.Upcall;

namespace Aster;

/// <summary>HTTPS and optional WebTransport listener configuration (ABI 4.12).</summary>
public sealed record HttpListenerOptions
{
    /// <summary>An IP socket address, with port zero selecting an ephemeral port.</summary>
    public string BindAddress { get; init; } = "127.0.0.1:0";
    public byte[] CertificatePem { get; init; } = [];
    public byte[] PrivateKeyPem { get; init; } = [];
    /// <summary>Also bind QUIC on the TCP listener's exact address and port.</summary>
    public bool WebTransport { get; init; }
    /// <summary>Pending authentication callbacks per listener; zero selects 256.</summary>
    public uint MaxPendingAuth { get; init; }
    /// <summary>Encoded request/response byte limit; zero selects 65536.</summary>
    public uint MaxAuthPayloadBytes { get; init; }
}

/// <summary>
/// Owns HTTPS/optional WebTransport sockets and retains its runtime.
/// Dispose before its server and, for authenticated serving, upcall table. Server disposal also stops its native
/// children; their managed wrappers still require disposal to release runtime ownership.
/// </summary>
public sealed class AsterHttpListener : IAsyncDisposable, IDisposable
{
    private readonly Runtime _runtime;
    // Keep dependencies reachable until the wrapper releases its owner.
    private readonly AsterServer _server;
    private readonly Upcalls? _upcalls;
    private readonly ulong _handle;
    private readonly SemaphoreSlim _stopGate = new(1, 1);
    private bool _closed;

    internal AsterHttpListener(Runtime retainedRuntime, AsterServer server, Upcalls? upcalls, ulong handle)
        => (_runtime, _server, _upcalls, _handle) = (retainedRuntime, server, upcalls, handle);

    /// <summary>Bound HTTPS address, including the assigned port; ready on return from start.</summary>
    public string TcpAddress { get; private set; } = "";
    /// <summary>Bound WebTransport address, or null when disabled.</summary>
    public string? QuicAddress { get; private set; }

    internal static ulong StartNative(Runtime runtime, ulong server, ulong upcalls, HttpListenerOptions options, bool publicMode)
    {
        using var scope = new NativeScope();
        var config = new HttpServerConfigNative
        {
            struct_size = (uint)Marshal.SizeOf<HttpServerConfigNative>(),
            flags = options.WebTransport ? Native.HttpServerWebTransport : 0,
            bind_addr = scope.Utf8(options.BindAddress),
            cert_pem = scope.Bytes(options.CertificatePem),
            key_pem = scope.Bytes(options.PrivateKeyPem),
            max_pending_auth = options.MaxPendingAuth,
            max_auth_payload_bytes = options.MaxAuthPayloadBytes,
        };
        ulong handle;
        if (publicMode)
            Native.Check(Native.aster_server_http_start_public(runtime.Handle, server, in config, out handle), "aster_server_http_start_public");
        else
            Native.Check(Native.aster_server_http_start(runtime.Handle, server, upcalls, in config, out handle), "aster_server_http_start");
        return handle;
    }

    internal void ReadAddresses(bool webTransport)
    {
        TcpAddress = ReadAddress(false);
        QuicAddress = webTransport ? ReadAddress(true) : null;
    }

    private unsafe string ReadAddress(bool quic)
    {
        int status = quic
            ? Native.aster_http_listener_quic_addr(_runtime.Handle, _handle, out ulong buffer, out IntPtr ptr, out uint len)
            : Native.aster_http_listener_tcp_addr(_runtime.Handle, _handle, out buffer, out ptr, out len);
        try
        {
            Native.Check(status, "HTTP listener address");
            return Encoding.UTF8.GetString(new ReadOnlySpan<byte>((void*)ptr, checked((int)len)));
        }
        finally { if (buffer != 0) _runtime.ReleaseBuffer(buffer); }
    }

    /// <summary>
    /// Stop accepting, drain application calls within grace, cancel remaining calls,
    /// then join protocol/socket cleanup. QUIC draining can extend beyond grace.
    /// Native failures preserve ownership for retry. Cancellation is honored before
    /// the native stop begins; once begun this method awaits joined cleanup.
    /// </summary>
    public async Task StopAsync(TimeSpan grace, CancellationToken cancellationToken = default)
    {
        if (grace < TimeSpan.Zero) throw new ArgumentOutOfRangeException(nameof(grace));
        await _stopGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            if (_closed) return;
            cancellationToken.ThrowIfCancellationRequested();
            uint milliseconds = (uint)Math.Min(uint.MaxValue, Math.Ceiling(grace.TotalMilliseconds));
            await Task.Run(() =>
            {
                int status = Native.aster_http_listener_stop(_runtime.Handle, _handle, milliseconds);
                if (status != (int)Status.NotFound && status != (int)Status.AlreadyClosed)
                    Native.Check(status, "aster_http_listener_stop");
            }, CancellationToken.None).ConfigureAwait(false);
            _closed = true;
            _runtime.Release();
        }
        finally { _stopGate.Release(); }
    }

    /// <summary>Cancel application work immediately, then join protocol/socket cleanup.</summary>
    public ValueTask DisposeAsync() => new(StopAsync(TimeSpan.Zero));
    public void Dispose() => DisposeAsync().AsTask().GetAwaiter().GetResult();
}

/// <summary>Startup failed and cleanup also failed. Dispose Listener to retry owned cleanup.</summary>
public sealed class HttpListenerStartException : Exception
{
    internal HttpListenerStartException(AsterHttpListener listener, Exception cause, Exception cleanup)
        : base("HTTP listener startup cleanup failed; dispose Listener to retry", new AggregateException(cause, cleanup)) => Listener = listener;
    public AsterHttpListener Listener { get; }
}

public sealed partial class AsterServer
{
    private readonly SemaphoreSlim _httpLifecycleGate = new(1, 1);

    /// <summary>
    /// Start authenticated HTTPS/optional WebTransport using a same-runtime upcall
    /// table with ServeRpcAuthenticator registered first. Returns after all requested
    /// sockets bind. Inputs are copied; cancellation during native startup awaits
    /// forced cleanup rather than abandoning a late-created owner. If cleanup fails,
    /// HttpListenerStartException.Listener preserves the owner for retry.
    /// </summary>
    public async Task<AsterHttpListener> StartHttpListenerAsync(HttpListenerOptions options, Upcalls upcalls,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        ArgumentNullException.ThrowIfNull(upcalls);
        cancellationToken.ThrowIfCancellationRequested();
        upcalls.RequireRpcAuthenticator(_node.Runtime);
        return await StartHttpListenerCore(options, upcalls, false, cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// Explicitly serve public HTTPS/optional WebTransport without an authenticator.
    /// Public calls have empty attributes and no Iroh-authenticated provenance.
    /// Gate 3 and Iroh-only requirements still apply; session-scoped services are
    /// rejected before their factory runs. Authentication limits must be zero.
    /// Readiness, cancellation and cleanup follow StartHttpListenerAsync.
    /// </summary>
    public async Task<AsterHttpListener> StartPublicHttpListenerAsync(HttpListenerOptions options,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        cancellationToken.ThrowIfCancellationRequested();
        if (options.MaxPendingAuth != 0 || options.MaxAuthPayloadBytes != 0)
            throw new ArgumentException("public listeners do not accept authentication limits", nameof(options));
        return await StartHttpListenerCore(options, null, true, cancellationToken).ConfigureAwait(false);
    }

    private async Task<AsterHttpListener> StartHttpListenerCore(HttpListenerOptions options, Upcalls? upcalls,
        bool publicMode, CancellationToken cancellationToken)
    {
        if (string.IsNullOrEmpty(options.BindAddress)) throw new ArgumentException("BindAddress must be nonempty", nameof(options));
        if (options.CertificatePem is not { Length: > 0 } || options.PrivateKeyPem is not { Length: > 0 })
            throw new ArgumentException("certificate and private key PEM are required", nameof(options));
        var snapshot = options with { CertificatePem = options.CertificatePem.ToArray(), PrivateKeyPem = options.PrivateKeyPem.ToArray() };
        await _httpLifecycleGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
            cancellationToken.ThrowIfCancellationRequested();
            var runtime = _node.Runtime.Retain();
            ulong handle;
            try
            {
                handle = await Task.Run(() =>
                {
                    cancellationToken.ThrowIfCancellationRequested();
                    return AsterHttpListener.StartNative(runtime, _dispatch.NativeHandle, upcalls?.NativeHandle ?? 0, snapshot, publicMode);
                }, CancellationToken.None).ConfigureAwait(false);
            }
            catch { runtime.Release(); throw; }
            var listener = new AsterHttpListener(runtime, this, upcalls, handle);
            try
            {
                cancellationToken.ThrowIfCancellationRequested();
                listener.ReadAddresses(snapshot.WebTransport);
                cancellationToken.ThrowIfCancellationRequested();
                return listener;
            }
            catch (Exception cause)
            {
                try { await listener.DisposeAsync().ConfigureAwait(false); }
                catch (Exception cleanup) { throw new HttpListenerStartException(listener, cause, cleanup); }
                throw;
            }
        }
        finally { _httpLifecycleGate.Release(); }
    }
}
