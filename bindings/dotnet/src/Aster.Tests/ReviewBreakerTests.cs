using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

public class ReviewBreakerTests
{
    private static CircuitBreakerInterceptor Open(double recoveryTimeout)
    {
        var b = new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: recoveryTimeout);
        b.RecordFailure(new RpcError(StatusCode.Unavailable, "down"));
        return b;
    }

    [Fact]
    public async Task Refusal_Releases_Probe_Already_Admitted_By_Another_Breaker()
    {
        var probe = Open(0);
        var refusing = Open(3600);
        var chain = new ClientInterceptors([probe, refusing]);
        var ctx = chain.NewContext("Echo", "echo", null, "unary", null);
        var error = await Assert.ThrowsAsync<RpcError>(() => chain.RunAsync<int, int>(
            ctx, 1, null, true,
            (_, _) => throw new InvalidOperationException("refused call reached transport"),
            CancellationToken.None));
        Assert.Equal(StatusCode.Unavailable, error.Code);
        probe.BeforeCall(); // Must admit: the previous attempt never reached the peer.
    }

    [Fact]
    public async Task Caller_Cancellation_Releases_Half_Open_Probe()
    {
        var probe = Open(0);
        var chain = new ClientInterceptors([probe]);
        var ctx = chain.NewContext("Echo", "echo", null, "unary", null);
        using var cancel = new CancellationTokenSource();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => chain.RunAsync<int, int>(
            ctx, 1, null, true,
            (_, _) => { cancel.Cancel(); return Task.FromCanceled<int>(cancel.Token); },
            cancel.Token));
        probe.BeforeCall(); // Cancellation says nothing about peer health.
    }
}
