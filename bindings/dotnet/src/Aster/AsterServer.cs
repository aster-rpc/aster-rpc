using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Aster.Codec;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster;

/// <summary>
/// An Aster RPC server: starts a <see cref="Aster.Node"/> from an <see cref="AsterConfig"/>,
/// attaches a reactor on the <c>aster/1</c> ALPN and dispatches calls to
/// <see cref="ServiceDefinition"/> handlers. Every call ends with an AEv1 <c>RpcStatus</c>
/// trailer; a handler exception becomes <c>INTERNAL</c> (or the code of an
/// <see cref="RpcError"/>).
/// <code>
/// await using var server = await AsterServer.StartAsync(new AsterServerOptions
/// {
///     Config = AsterConfig.FromEnv() with { AllowAllConsumers = true },
///     Services = { service },
/// });
/// Console.WriteLine(await server.TicketAsync());
/// </code>
/// </summary>
public sealed class AsterServer : IAsyncDisposable, IDisposable
{
    public const string AsterAlpn = "aster/1";
    /// <summary>The consumer-admission ALPN; kept open by the default gate.</summary>
    public const string ConsumerAdmissionAlpn = "aster.consumer_admission";

    private readonly Node _node;
    private readonly Gate? _ownedGate;
    private readonly Reactor _reactor;
    private readonly ICodec _codec;
    private readonly Dictionary<string, ServiceDefinition> _services;
    private readonly IReadOnlyList<IInterceptor> _interceptors;
    private readonly Thread _pollThread;
    private readonly CancellationTokenSource _stopping = new();
    private int _inFlight;
    private int _disposed;

    private AsterServer(Node node, Gate? ownedGate, Gate gate, Reactor reactor, ICodec codec,
        Dictionary<string, ServiceDefinition> services, IReadOnlyList<IInterceptor> interceptors)
    {
        _interceptors = interceptors;
        _node = node;
        _ownedGate = ownedGate;
        Gate = gate;
        _reactor = reactor;
        _codec = codec;
        _services = services;
        _pollThread = new Thread(PollLoop) { Name = "aster-server-poll", IsBackground = true };
        _pollThread.Start();
    }

    /// <summary>Starts a server.</summary>
    public static async Task<AsterServer> StartAsync(AsterServerOptions options, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        var services = new Dictionary<string, ServiceDefinition>(StringComparer.Ordinal);
        foreach (var s in options.Services)
            if (!services.TryAdd(s.Name, s))
                throw new ArgumentException($"service {s.Name} registered twice");

        ICodec codec = options.Codec ?? new ForyCodec();
        if (codec is ForyCodec fory)
            foreach (var m in services.Values.SelectMany(s => s.Methods.Values))
                m.RegisterTypes(fory);

        var config = options.Config ?? new AsterConfig();
        Gate? ownedGate = null;
        Gate gate = options.Gate ?? (ownedGate = Gate.Create()
            .SetTrustMode(config.AllowAllConsumers ? TrustMode.OpenDev : TrustMode.Protected)
            .SetAdmissionAlpns([ConsumerAdmissionAlpn]));
        Node? node = null;
        try
        {
            node = await Node.StartAsync(config, gate, [AsterAlpn, ConsumerAdmissionAlpn], cancellationToken).ConfigureAwait(false);
            var reactor = new Reactor(node.Runtime.Handle, node.Handle.Value, options.RingCapacity);
            return new AsterServer(node, ownedGate, gate, reactor, codec, services, options.Interceptors.ToList());
        }
        catch
        {
            node?.Dispose();
            ownedGate?.Dispose();
            throw;
        }
    }

    /// <summary>The server's node.</summary>
    public Node Node => _node;

    /// <summary>The Gate-0 policy guarding the node (admit peers here unless dev mode is on).</summary>
    public Gate Gate { get; }

    /// <summary>The server's node id (64 hex).</summary>
    public string NodeId() => _node.NodeId();

    /// <summary>A ticket for this server (waits briefly for direct addresses).</summary>
    public Task<AsterTicket> TicketAsync(CancellationToken cancellationToken = default)
        => _node.TicketAsync(TimeSpan.FromSeconds(5), cancellationToken);

    /// <summary>The registered services by name.</summary>
    public IReadOnlyDictionary<string, ServiceDefinition> Services => _services;

    /// <summary>
    /// Raised (on the poll thread) when the reactor reports a connection closed. Per-connection
    /// state keyed on <see cref="ServerCallContext.ConnectionId"/> can be dropped here.
    /// </summary>
    public event Action<ulong>? ConnectionClosed;

    private void PollLoop()
    {
        while (!_stopping.IsCancellationRequested)
        {
            ReactorCall[] calls;
            try { calls = _reactor.Poll(32, 100); }
            catch (ObjectDisposedException) { return; }
            catch (Exception ex)
            {
                Console.Error.WriteLine($"aster reactor poll failed: {ex.Message}");
                Thread.Sleep(10);
                continue;
            }

            foreach (var call in calls)
            {
                if (call.Kind == ReactorEventKind.ConnectionClosed)
                {
                    try { ConnectionClosed?.Invoke(call.ConnectionId); }
                    catch (Exception ex) { Console.Error.WriteLine($"ConnectionClosed handler failed: {ex.Message}"); }
                    continue;
                }
                if (call.Kind != ReactorEventKind.Call)
                    continue;
                // Handlers and submits run off the poll thread: a QUEUE_FULL submit waits for a
                // RESPONSE_WRITABLE event that only this loop can deliver.
                Interlocked.Increment(ref _inFlight);
                _ = Task.Run(() => DispatchAsync(call));
            }
        }
    }

    private async Task DispatchAsync(ReactorCall inbound)
    {
        ServerCall? call = null;
        CancellationTokenSource? cts = null;
        try
        {
            StreamHeader header;
            try { header = AsterEnvelope.DecodeStreamHeader(inbound.Header); }
            catch (Exception e)
            {
                await Fail(inbound.CallId, new RpcStatus(StatusCode.InvalidArgument, $"malformed stream header: {e.Message}")).ConfigureAwait(false);
                return;
            }
            if (header.SerializationMode != SerializationMode.Xlang)
            {
                await Fail(inbound.CallId, new RpcStatus(StatusCode.InvalidArgument,
                    $"unsupported serialization mode: {(int)header.SerializationMode}")).ConfigureAwait(false);
                return;
            }
            if (!_services.TryGetValue(header.Service, out var service))
            {
                await Fail(inbound.CallId, new RpcStatus(StatusCode.Unimplemented, $"unknown service: {header.Service}")).ConfigureAwait(false);
                return;
            }
            if (!service.Methods.TryGetValue(header.Method, out var method))
            {
                await Fail(inbound.CallId, new RpcStatus(StatusCode.Unimplemented,
                    $"unknown method: {header.Service}/{header.Method}")).ConfigureAwait(false);
                return;
            }
            if (header.SessionId != 0)
            {
                await Fail(inbound.CallId, new RpcStatus(StatusCode.FailedPrecondition,
                    $"service {header.Service} is shared-scoped but the call carries session {header.SessionId}")).ConfigureAwait(false);
                return;
            }

            cts = CancellationTokenSource.CreateLinkedTokenSource(_stopping.Token);
            DateTimeOffset? deadline = null;
            if (header.Deadline > 0)
            {
                deadline = DateTimeOffset.UtcNow.AddSeconds(header.Deadline);
                cts.CancelAfter(TimeSpan.FromSeconds(header.Deadline));
            }
            var ctx = new ServerCallContext(header, method.Pattern, inbound.PeerId, inbound.ConnectionId, deadline, cts.Token);
            call = new ServerCall(_reactor, inbound.CallId, inbound.Request, inbound.RequestFlags, _codec, ctx);
            if (_interceptors.Count > 0)
            {
                // Request-side hooks see the raw (logical) first request payload, as in the
                // Java server; a thrown RpcError rejects the call with its code.
                InterceptorChain.ApplyRequest(_interceptors, InterceptorContext(header, method, inbound, deadline), call.FirstRequest);
            }
            await method.InvokeAsync(call).ConfigureAwait(false);
        }
        catch (Exception e)
        {
            RpcStatus status = cts is { IsCancellationRequested: true } && !_stopping.IsCancellationRequested && e is OperationCanceledException
                ? new RpcStatus(StatusCode.DeadlineExceeded, "deadline exceeded")
                : RpcStatus.FromException(e);
            if (call != null)
            {
                try { await call.CompleteAsync(status).ConfigureAwait(false); }
                catch (Exception ex) { LogSubmitFailure(inbound.CallId, ex); }
            }
            else
            {
                await Fail(inbound.CallId, status).ConfigureAwait(false);
            }
        }
        finally
        {
            cts?.Dispose();
            Interlocked.Decrement(ref _inFlight);
        }
    }

    private static CallContext InterceptorContext(StreamHeader header, MethodDefinition method, ReactorCall inbound, DateTimeOffset? deadline)
        => new()
        {
            Service = header.Service,
            Method = header.Method,
            CallId = inbound.CallId.ToString(System.Globalization.CultureInfo.InvariantCulture),
            Peer = inbound.PeerId,
            Metadata = header.Metadata.GroupBy(kv => kv.Key).ToDictionary(g => g.Key, g => g.Last().Value),
            Deadline = deadline?.ToUnixTimeMilliseconds() / 1000.0,
            IsStreaming = method.Pattern != MethodPattern.Unary,
            Pattern = method.Pattern switch
            {
                MethodPattern.Unary => "unary",
                MethodPattern.ServerStream => "server_stream",
                MethodPattern.ClientStream => "client_stream",
                _ => "bidi_stream",
            },
            Idempotent = method.Idempotent,
        };

    private async Task Fail(ulong callId, RpcStatus status)
    {
        try { await _reactor.SubmitTrailerAsync(callId, ServerCall.TrailerFrame(status)).ConfigureAwait(false); }
        catch (Exception ex) { LogSubmitFailure(callId, ex); }
    }

    // e.g. NOT_FOUND: the peer cancelled/disconnected, or the response channel closed.
    private static void LogSubmitFailure(ulong callId, Exception ex)
        => Console.Error.WriteLine($"aster submit for call {callId} failed: {ex.Message}");

    /// <summary>Stops dispatching, waits briefly for in-flight calls, then closes the node.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _stopping.Cancel();
        _pollThread.Join(2000);
        for (int i = 0; i < 50 && Volatile.Read(ref _inFlight) > 0; i++)
            await Task.Delay(20).ConfigureAwait(false);
        _reactor.Dispose();
        await _node.DisposeAsync().ConfigureAwait(false);
        _ownedGate?.Dispose();
        _stopping.Dispose();
    }

    public void Dispose() => DisposeAsync().AsTask().GetAwaiter().GetResult();
}

/// <summary>Options for <see cref="AsterServer.StartAsync"/>.</summary>
public sealed class AsterServerOptions
{
    /// <summary>Node configuration (null = defaults).</summary>
    public AsterConfig? Config { get; init; }

    /// <summary>The services to serve.</summary>
    public IList<ServiceDefinition> Services { get; init; } = new List<ServiceDefinition>();

    /// <summary>Payload codec (default: a <see cref="ForyCodec"/>; service types are registered on it).</summary>
    public ICodec? Codec { get; init; }

    /// <summary>
    /// Gate-0 policy. Default: a gate owned by the server, <see cref="TrustMode.OpenDev"/> when
    /// <see cref="AsterConfig.AllowAllConsumers"/> is set, else <see cref="TrustMode.Protected"/>
    /// (only admitted peers reach <c>aster/1</c>).
    /// </summary>
    public Gate? Gate { get; init; }

    /// <summary>
    /// Server interceptors; their <see cref="IInterceptor.OnRequest"/> runs before every handler
    /// (with the raw request payload) and may reject the call by throwing <see cref="RpcError"/>.
    /// </summary>
    public IList<IInterceptor> Interceptors { get; init; } = new List<IInterceptor>();

    /// <summary>Reactor SPSC ring capacity.</summary>
    public uint RingCapacity { get; init; } = 256;
}
