using System;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Health;

/// <summary>Where a <see cref="HealthServer"/> listens.</summary>
public sealed record HealthOptions
{
    /// <summary>TCP port; 0 (default) picks an ephemeral one (read it from <see cref="HealthServer.Address"/>).</summary>
    public ushort Port { get; init; }

    /// <summary>Bind host. Default <c>127.0.0.1</c>; pods that are probed from outside need <c>0.0.0.0</c>.</summary>
    public string Host { get; init; } = "127.0.0.1";
}

/// <summary>
/// The ops listener (Rust <c>aster::ops</c>): <c>GET /healthz</c> (liveness), <c>GET /readyz</c>
/// (readiness, with named checks) and <c>GET /metrics</c> (Prometheus text from the OpenTelemetry pull
/// reader: Aster's built-ins and the instruments of <see cref="Observability.Metrics"/>). Starting one
/// joins the observability pipeline (or starts an environment-only one) and makes sure it has the pull
/// reader, so <c>/metrics</c> is never empty.
/// <para>
/// Readiness is false until a server is attached (<see cref="AsterServerOptions.Health"/> does that:
/// ready after a successful start, not ready first on dispose) or <see cref="SetReady"/> says so.
/// Named checks start failing: a declared dependency is not ready until it says so. Liveness ignores
/// checks.
/// </para>
/// </summary>
public sealed unsafe class HealthServer : IDisposable, IAsyncDisposable
{
    private readonly Runtime _runtime;
    private ulong _handle;

    private HealthServer(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>Starts the listener on <paramref name="node"/> (blocks until it is bound).</summary>
    public static HealthServer Start(Node node, HealthOptions? options = null)
    {
        ArgumentNullException.ThrowIfNull(node);
        options ??= new HealthOptions();
        using var scope = new NativeScope();
        var cfg = new HealthConfigNative
        {
            struct_size = (uint)sizeof(HealthConfigNative),
            port = options.Port,
            host = scope.Utf8(options.Host),
        };
        ulong h;
        Native.Check(Native.aster_health_start(node.Runtime.Handle, node.Handle.Value, &cfg, &h), "aster_health_start");
        return new HealthServer(node.Runtime, h);
    }

    /// <summary>Starts the listener without blocking the caller.</summary>
    public static Task<HealthServer> StartAsync(Node node, HealthOptions? options = null)
        => Task.Run(() => Start(node, options));

    /// <summary>The native handle (<c>aster_health_t</c>).</summary>
    internal ulong Handle => _handle;

    /// <summary>The bound <c>ip:port</c>.</summary>
    public string Address
    {
        get
        {
            Bytes b = default;
            Native.Check(Native.aster_health_addr(Live(), &b), "aster_health_addr");
            return Encoding.UTF8.GetString(NativeScope.TakeOwned(ref b));
        }
    }

    /// <summary>The base URL (<c>http://ip:port</c>).</summary>
    public string BaseUrl => "http://" + Address;

    /// <summary>Sets readiness (<c>/readyz</c>).</summary>
    public void SetReady(bool ready) => Native.Check(Native.aster_health_set_ready(Live(), ready), "aster_health_set_ready");

    /// <summary>Sets liveness (<c>/healthz</c>).</summary>
    public void SetHealthy(bool healthy) => Native.Check(Native.aster_health_set_healthy(Live(), healthy), "aster_health_set_healthy");

    /// <summary>Sets a named readiness check (created failing, <c>/readyz</c> lists it under <c>checks</c>).</summary>
    public void SetCheck(string name, bool ok)
    {
        ArgumentException.ThrowIfNullOrEmpty(name);
        using var scope = new NativeScope();
        Native.Check(Native.aster_health_check_set(Live(), scope.Utf8(name), ok), "aster_health_check_set");
    }

    /// <summary>A handle on one named check: <c>health.Check("db").Set(true)</c>.</summary>
    public HealthCheck Check(string name)
    {
        ArgumentException.ThrowIfNullOrEmpty(name);
        SetCheck(name, false);
        return new HealthCheck(this, name);
    }

    private ulong Live()
    {
        ulong h = Volatile.Read(ref _handle);
        ObjectDisposedException.ThrowIf(h == 0, this);
        return h;
    }

    /// <summary>Stops the listener (the port is free on return) and releases its pipeline reference.</summary>
    public void Dispose()
    {
        ulong h = Interlocked.Exchange(ref _handle, 0);
        if (h != 0) Native.aster_health_stop(_runtime.Handle, h);
    }

    /// <inheritdoc cref="Dispose"/>
    public ValueTask DisposeAsync()
    {
        Dispose();
        return ValueTask.CompletedTask;
    }
}

/// <summary>One named readiness check of a <see cref="HealthServer"/>.</summary>
public sealed class HealthCheck
{
    private readonly HealthServer _server;

    internal HealthCheck(HealthServer server, string name)
    {
        _server = server;
        Name = name;
    }

    /// <summary>The check's name.</summary>
    public string Name { get; }

    /// <summary>Marks the dependency ready (true) or failing (false).</summary>
    public void Set(bool ok) => _server.SetCheck(Name, ok);
}
