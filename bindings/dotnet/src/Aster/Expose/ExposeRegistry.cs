using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Upcall;

namespace Aster.Expose;

/// <summary>
/// An expose registry on a node (<c>aster_expose_t</c>, C ABI 4.2): the services published to
/// consumers over the node's ALPN, either a raw socket splice or a foreign HTTP handler, plus an
/// optional edge route policy. Deliver connections with <see cref="ServeAsync"/> or
/// <see cref="HandleConnection"/>.
///
/// <para>Authorization: <see cref="ExposeSocket"/> and <see cref="ExposeHttp"/> put every
/// <c>(connection, service)</c> admission to the binding as a kind-48 decision when given an upcall
/// table (register the decision with <see cref="Upcalls.ServeExposeAuthorizer"/>); without one a
/// socket service admits <b>every</b> peer that reaches the node's ALPN, so do that only when the
/// node itself is access-controlled (Gate 0). HTTP services always need a table, and a table without
/// an authorizer denies (fail-closed default).</para>
/// </summary>
public sealed class ExposeRegistry : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private readonly Dictionary<string, HttpRouter> _httpServices = new();
    private int _disposed;

    private ExposeRegistry(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>A registry for <paramref name="node"/> (which must have been started with the ALPN(s) the consumers connect on).</summary>
    public static ExposeRegistry Create(Node node)
    {
        ArgumentNullException.ThrowIfNull(node);
        Native.Check(Native.aster_expose_new(node.Runtime.Handle, node.Handle.Value, out ulong h), "aster_expose_new");
        return new ExposeRegistry(node.Runtime, h);
    }

    /// <summary>
    /// Exposes a raw L4 splice to <paramref name="address"/> (<c>"ip:port"</c>, a loopback server you run) under
    /// <paramref name="serviceId"/>, node-wide. Re-registering an id replaces it.
    /// </summary>
    public void ExposeSocket(string serviceId, string address, Upcalls? authorizer = null)
    {
        ThrowIfDisposed();
        using var scope = new NativeScope();
        Native.Check(Native.aster_expose_socket(_runtime.Handle, _handle, scope.Utf8(serviceId), scope.Utf8(address), authorizer?.NativeHandle ?? 0), "aster_expose_socket");
    }

    /// <summary>
    /// Exposes <paramref name="serviceId"/> as an HTTP service handled by <paramref name="handler"/>:
    /// admission is a kind-48 decision and each request one kind-49 upcall on <paramref name="upcalls"/>,
    /// with bodies moved through <paramref name="bodies"/>. See design §6 for limits, cancellation and
    /// failure statuses (<c>ERROR</c> 500, <c>UNIMPLEMENTED</c> 501, bad head 502, overload 503,
    /// timeout 504). The head must be answered within the kind-49 timeout (default 5 s,
    /// <see cref="Upcalls.SetPolicy"/>); a streamed body has no deadline. Re-registering replaces.
    /// </summary>
    public void ExposeHttp(string serviceId, Upcalls upcalls, HttpBodies bodies, ExposeHttpHandler handler, HttpForeignOptions? options = null)
    {
        ThrowIfDisposed();
        ArgumentNullException.ThrowIfNull(handler);
        var router = HttpRouter.For(upcalls);
        router.Add(serviceId, bodies, handler, options?.WriteChunkSize ?? 64 * 1024);
        try
        {
            ExposeHttpForeign(serviceId, upcalls, bodies, options);
        }
        catch
        {
            router.Remove(serviceId);
            throw;
        }
        lock (_httpServices) _httpServices[serviceId] = router;
    }

    /// <summary>
    /// The native registration only (<c>aster_expose_http_foreign</c>): handle kind 49 yourself with
    /// <see cref="Upcalls.Handle"/> and move bodies with <paramref name="bodies"/>.
    /// </summary>
    public unsafe void ExposeHttpForeign(string serviceId, Upcalls upcalls, HttpBodies bodies, HttpForeignOptions? options = null)
    {
        ThrowIfDisposed();
        using var scope = new NativeScope();
        if (options == null)
        {
            Native.Check(Native.aster_expose_http_foreign(_runtime.Handle, _handle, upcalls.NativeHandle, bodies.NativeHandle, scope.Utf8(serviceId), null), "aster_expose_http_foreign");
            return;
        }
        HttpForeignOptionsNative o = options.ToNative();
        Native.Check(Native.aster_expose_http_foreign(_runtime.Handle, _handle, upcalls.NativeHandle, bodies.NativeHandle, scope.Utf8(serviceId), &o), "aster_expose_http_foreign");
    }

    /// <summary>
    /// Accepts route registrations (acts as an edge) node-wide, granting what the binding decides:
    /// each request is one kind-50 upcall on <paramref name="upcalls"/> (register the policy with
    /// <see cref="Upcalls.ServeRoutePolicy"/>). With no usable answer the kind's failure mode applies
    /// to the whole request: fail-closed (default) grants nothing, fail-open grants every route.
    /// </summary>
    public void ServeRoutePolicy(Upcalls upcalls)
    {
        ThrowIfDisposed();
        Native.Check(Native.aster_expose_route_policy_foreign(_runtime.Handle, _handle, upcalls.NativeHandle), "aster_expose_route_policy_foreign");
    }

    /// <summary>Stops exposing <paramref name="serviceId"/> (idempotent). Streams already being served finish.</summary>
    public void Unexpose(string serviceId)
    {
        ThrowIfDisposed();
        using var scope = new NativeScope();
        Native.Check(Native.aster_expose_unexpose(_runtime.Handle, _handle, scope.Utf8(serviceId)), "aster_expose_unexpose");
        lock (_httpServices)
            if (_httpServices.Remove(serviceId, out var router)) router.Remove(serviceId);
    }

    /// <summary>
    /// Feeds one inbound connection (from <see cref="Node.AcceptAsync"/>) to the registry, so its relay
    /// streams reach the exposed services. Call once per connection, before the peer opens streams on
    /// it; the connection stays usable and owned by the caller.
    /// </summary>
    public void HandleConnection(Connection connection)
    {
        ThrowIfDisposed();
        Native.Check(Native.aster_expose_handle_connection(_runtime.Handle, _handle, connection.Handle.Value), "aster_expose_handle_connection");
    }

    /// <summary>
    /// Owns the node's accept loop for expose: accepts inbound connections of <paramref name="node"/>
    /// and feeds each whose ALPN is in <paramref name="alpns"/> (empty = every ALPN) to the registry.
    /// Completes when the node closes (or <paramref name="cancellationToken"/> fires). Do not combine
    /// with <see cref="Node.AcceptAsync"/> on the same node: use <see cref="HandleConnection"/> there.
    /// </summary>
    public async Task ServeAsync(Node node, IReadOnlyList<string>? alpns = null, CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_expose_serve(_runtime.Handle, node.Handle.Value, _handle, scope.Utf8List(alpns ?? Array.Empty<string>()), 0, out op), "aster_expose_serve");
        try
        {
            await Operations.AwaitAsync(_runtime, op, EventKind.Error, "aster_expose_serve", cancellationToken).ConfigureAwait(false);
        }
        catch (IrohException)
        {
            // The loop ends with the accept error once the node closes.
        }
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);

    /// <summary>Frees the handle. Services registered on it keep serving connections already fed to it: <see cref="Unexpose"/> first if they must stop.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0)
            Native.aster_expose_free(_runtime.Handle, _handle);
    }
}

/// <summary>A buffered HTTP response from an exposed service (<see cref="ExposeClient.RelayRequestAsync"/>); any status, including the 5xx a failing handler maps to.</summary>
public sealed record ExposeRelayResponse(ushort Status, IReadOnlyList<HttpHeader> Headers, byte[] Body)
{
    public string BodyText => Encoding.UTF8.GetString(Body);
}

/// <summary>A live route registration: the edge keeps the granted routes while it is held.</summary>
public sealed class RouteRegistration : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _disposed;

    internal RouteRegistration(Runtime runtime, ulong handle, IReadOnlyList<uint> granted)
    {
        _runtime = runtime;
        _handle = handle;
        Granted = granted;
    }

    /// <summary>Indexes into the requested specs the edge granted, ascending.</summary>
    public IReadOnlyList<uint> Granted { get; }

    /// <summary>Ends the registration: the edge evicts its routes.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0 && _handle != 0)
            Native.aster_route_registration_free(_runtime.Handle, _handle);
    }
}

/// <summary>The consumer side of expose: calls over a connection to a node that exposes services. Needs no <see cref="ExposeRegistry"/>.</summary>
public static class ExposeClient
{
    /// <summary>
    /// Relays one buffered HTTP request to <paramref name="serviceId"/> exposed by the peer on
    /// <paramref name="connection"/> and collects the whole response (streaming relay is not in the
    /// C ABI). A transport failure, a refused request (e.g. denied by the peer's authorizer) or a
    /// response that failed mid-body is an <see cref="IrohException"/>; any HTTP status completes normally.
    /// </summary>
    public static async Task<ExposeRelayResponse> RelayRequestAsync(Connection connection, string serviceId, string method, string uri,
        IReadOnlyList<HttpHeader>? headers = null, byte[]? body = null, HttpProtocolVersion version = HttpProtocolVersion.Http11,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(connection);
        byte[] c4 = Ucv1.EncodeExposeHttpRequest(new ExposeHttpRequestWire(serviceId, null, 0, (byte)version, method, uri,
            headers ?? Array.Empty<HttpHeader>(), new HttpBody.Inline(body ?? Array.Empty<byte>())));
        var rt = connection.Runtime;
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_expose_relay_request(rt.Handle, connection.Handle.Value, scope.Bytes(c4), 0, out op), "aster_expose_relay_request");
        var (_, data) = await Operations.AwaitAsync(rt, op, EventKind.BytesResult, "aster_expose_relay_request", cancellationToken).ConfigureAwait(false);
        var wire = Ucv1.DecodeExposeHttpResponse(data);
        if (wire.Body is not HttpBody.Inline inline) throw new IrohException("relay_request answered a streamed body");
        return new ExposeRelayResponse(wire.Status, wire.Headers, inline.Body);
    }

    /// <summary>
    /// Asks the peer on <paramref name="connection"/> (an edge serving <see cref="ExposeRegistry.ServeRoutePolicy"/>)
    /// to route <paramref name="specs"/> to us, attaching <paramref name="metadata"/> for its policy.
    /// A refusal of the whole request is an <see cref="IrohException"/>. Dispose the registration to end it.
    /// </summary>
    public static async Task<RouteRegistration> RequestRouteAsync(Connection connection, IReadOnlyList<ExposeRouteSpec> specs,
        byte[]? metadata = null, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(connection);
        var rt = connection.Runtime;
        ulong op;
        unsafe
        {
            using var scope = new NativeScope();
            var arr = (RouteSpecNative*)scope.Alloc(sizeof(RouteSpecNative) * Math.Max(1, specs.Count));
            for (int i = 0; i < specs.Count; i++)
            {
                arr[i] = new RouteSpecNative
                {
                    host = scope.Utf8(specs[i].Host),
                    service_id = scope.Utf8(specs[i].ServiceId),
                    port = specs[i].Port,
                    protocol = specs[i].Protocol,
                };
            }
            Native.Check(Native.aster_expose_request_route(rt.Handle, connection.Handle.Value, specs.Count == 0 ? null : arr, (UIntPtr)specs.Count,
                scope.Bytes(metadata ?? Array.Empty<byte>()), 0, out op), "aster_expose_request_route");
        }
        var (ev, data) = await Operations.AwaitAsync(rt, op, EventKind.BytesResult, "aster_expose_request_route", cancellationToken).ConfigureAwait(false);
        try
        {
            return new RouteRegistration(rt, ev.handle, Ucv1.DecodeExposeRouteGrant(data));
        }
        catch
        {
            if (ev.handle != 0) Native.aster_route_registration_free(rt.Handle, ev.handle);
            throw;
        }
    }
}
