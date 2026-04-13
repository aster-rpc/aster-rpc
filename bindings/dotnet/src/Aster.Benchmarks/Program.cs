using System;
using System.IO;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using BenchmarkDotNet.Attributes;
using BenchmarkDotNet.Configs;
using BenchmarkDotNet.Running;

namespace Aster.Benchmarks;

/// <summary>
/// Performance Benchmarks (5b.8)
///
/// Measures .NET binding overhead for key operations:
/// - Submit latency (endpoint create, connect, open stream)
/// - CQ drain throughput (events/second)
/// - Memory per operation
/// - Small vs large payload overhead
/// </summary>
[MemoryDiagnoser]
public class RuntimeBenchmarks
{
    private const string ALPN = "bench";
    private Runtime? _runtime;
    private Endpoint? _endpoint;

    [GlobalSetup]
    public void Setup()
    {
        _runtime = new Runtime();
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        _endpoint = Endpoint.CreateAsync(_runtime, config).GetAwaiter().GetResult();
    }

    [GlobalCleanup]
    public void Cleanup()
    {
        _endpoint?.Dispose();
        _runtime?.Dispose();
    }

    [Benchmark]
    public void Runtime_Create_And_Dispose()
    {
        using var runtime = new Runtime();
    }

    [Benchmark]
    public void Endpoint_Create()
    {
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        using var runtime = new Runtime();
        var endpoint = Endpoint.CreateAsync(runtime, config).GetAwaiter().GetResult();
        endpoint.Dispose();
    }

    [Benchmark]
    public async Task Endpoint_Create_Async()
    {
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        using var runtime = new Runtime();
        var endpoint = await Endpoint.CreateAsync(runtime, config);
        endpoint.Dispose();
    }

    [Benchmark]
    public void NodeId_Retrieval()
    {
        _ = _endpoint!.NodeId();
    }

    [Benchmark]
    public async Task Connect_Accept_Baseline()
    {
        using var runtimeA = new Runtime();
        using var runtimeB = new Runtime();

        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var epA = await Endpoint.CreateAsync(runtimeA, config);
        var epB = await Endpoint.CreateAsync(runtimeB, config);

        string epAId = epA.NodeId();

        using var cts = new CancellationTokenSource(10000);
        var acceptTask = epA.AcceptAsync();
        await Task.Delay(50);
        var connB = await epB.ConnectAsync(epAId, ALPN, cts.Token);
        var connA = await acceptTask;

        connB.Close();
        connA.Close();
        epA.Dispose();
        epB.Dispose();
    }
}

/// <summary>
/// Stream benchmarks
/// </summary>
[MemoryDiagnoser]
public class StreamBenchmarks
{
    private const string ALPN = "bench";
    private static readonly TimeSpan Timeout = TimeSpan.FromSeconds(30);
    private Runtime? _runtimeA;
    private Runtime? _runtimeB;
    private Endpoint? _epA;
    private Endpoint? _epB;
    private Connection? _connA;
    private Connection? _connB;

    [GlobalSetup]
    public async Task Setup()
    {
        _runtimeA = new Runtime();
        _runtimeB = new Runtime();

        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        _epA = await Endpoint.CreateAsync(_runtimeA, config);
        _epB = await Endpoint.CreateAsync(_runtimeB, config);

        string epAId = _epA.NodeId();

        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var acceptTask = _epA.AcceptAsync();
        await Task.Delay(50);
        _connB = await _epB.ConnectAsync(epAId, ALPN, cts.Token);
        _connA = await acceptTask;
    }

    [GlobalCleanup]
    public void Cleanup()
    {
        try { _connB?.Close(); } catch { }
        try { _connA?.Close(); } catch { }
        _epA?.Dispose();
        _epB?.Dispose();
        _runtimeA?.Dispose();
        _runtimeB?.Dispose();
    }

    [Benchmark]
    public async Task OpenBi_Stream()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (send, recv) = await _connB!.OpenBiAsync(cts.Token);
        send.Dispose();
        recv.Dispose();
    }

    [Benchmark]
    public async Task AcceptBi_Stream()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (send, recv) = await _connA!.AcceptBiAsync(cts.Token);
        send.Dispose();
        recv.Dispose();
    }

    [Benchmark]
    public async Task Send_1KB()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (send, recv) = await _connB!.OpenBiAsync(cts.Token);

        byte[] data = Encoding.UTF8.GetBytes(new string('x', 1024));
        await send.SendAsync(data, cts.Token);
        await send.FinishAsync(cts.Token);

        send.Dispose();
        recv.Dispose();
    }

    [Benchmark]
    public async Task Send_64KB()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (send, recv) = await _connB!.OpenBiAsync(cts.Token);

        byte[] data = new byte[65536];
        new Random(42).NextBytes(data);
        await send.SendAsync(data, cts.Token);
        await send.FinishAsync(cts.Token);

        send.Dispose();
        recv.Dispose();
    }

    [Benchmark]
    public async Task SendReceive_RoundTrip()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (sendB, recvB) = await _connB!.OpenBiAsync(cts.Token);
        var (sendA, recvA) = await _connA!.AcceptBiAsync(cts.Token);

        byte[] data = Encoding.UTF8.GetBytes("Hello, Benchmark!");
        await sendB.SendAsync(data, cts.Token);
        await sendB.FinishAsync(cts.Token);

        byte[] received = await recvA.ReadAsync(4096, cts.Token);
        await sendA.SendAsync(received, cts.Token);
        await sendA.FinishAsync(cts.Token);

        byte[] echo = await recvB.ReadAsync(4096, cts.Token);

        sendB.Dispose();
        recvB.Dispose();
        sendA.Dispose();
        recvA.Dispose();
    }
}

/// <summary>
/// Memory benchmarks - measures allocations per operation
/// </summary>
[MemoryDiagnoser]
public class MemoryBenchmarks
{
    private const string ALPN = "bench";

    [Benchmark]
    public void Alloc_Per_Runtime()
    {
        using var runtime = new Runtime();
    }

    [Benchmark]
    public async Task Alloc_Per_Endpoint()
    {
        using var runtime = new Runtime();
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var endpoint = await Endpoint.CreateAsync(runtime, config);
        endpoint.Dispose();
    }

    [Benchmark]
    public void Bytes_FromArray_Overhead()
    {
        byte[] data = new byte[100];
        var b = Bytes.FromArray(data);
        _ = b.ToArray();
    }

    [Benchmark]
    public void Bytes_ToUtf8String_Overhead()
    {
        byte[] utf8 = Encoding.UTF8.GetBytes("Hello, Aster! This is a test string.");
        var b = Bytes.FromArray(utf8);
        _ = b.ToUtf8String();
    }
}

public class Program
{
    public static void Main(string[] args)
    {
        var summary = BenchmarkRunner.Run<RuntimeBenchmarks>();
        Console.WriteLine(summary);

        summary = BenchmarkRunner.Run<StreamBenchmarks>();
        Console.WriteLine(summary);

        summary = BenchmarkRunner.Run<MemoryBenchmarks>();
        Console.WriteLine(summary);
    }
}
