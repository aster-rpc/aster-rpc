namespace Aster.Interceptors;

/// <summary>
/// Token bucket rate limiter interceptor.
/// Limits request rate per service, per method, per peer, or globally.
/// Rejects requests that exceed the limit with ResourceExhausted status.
/// </summary>
public class RateLimitInterceptor : IInterceptor
{
    private readonly double _rate;
    private readonly double? _burst;
    private readonly string _per;
    private readonly Dictionary<string, TokenBucket> _buckets = new();
    private readonly TokenBucket _globalBucket;

    /// <param name="rate">Maximum requests per second.</param>
    /// <param name="burst">Maximum burst size (defaults to rate).</param>
    /// <param name="per">Granularity: "global", "service", "method", or "peer".</param>
    public RateLimitInterceptor(double rate = 100.0, double? burst = null, string per = "global")
    {
        _rate = rate;
        _burst = burst;
        _per = per;
        _globalBucket = new TokenBucket(rate, burst ?? rate);
    }

    public object OnRequest(CallContext ctx, object request)
    {
        var bucket = GetBucket(ctx);
        if (!bucket.TryAcquire())
        {
            throw new RpcError(
                StatusCode.ResourceExhausted,
                $"Rate limit exceeded ({_rate}/s per {_per})");
        }
        return request;
    }

    private TokenBucket GetBucket(CallContext ctx)
    {
        if (_per == "global")
            return _globalBucket;

        var key = _per switch
        {
            "service" => ctx.Service,
            "method" => $"{ctx.Service}.{ctx.Method}",
            "peer" => ctx.Peer ?? "unknown",
            _ => "global"
        };

        if (key == "global")
            return _globalBucket;

        if (!_buckets.TryGetValue(key, out var bucket))
        {
            bucket = new TokenBucket(_rate, _burst ?? _rate);
            _buckets[key] = bucket;
        }
        return bucket;
    }

    /// <summary>
    /// Simple token bucket rate limiter.
    /// </summary>
    private sealed class TokenBucket
    {
        private readonly double _rate;
        private readonly double _capacity;
        private double _tokens;
        private long _lastRefillTicks;

        public TokenBucket(double rate, double capacity)
        {
            _rate = rate;
            _capacity = capacity;
            _tokens = capacity;
            _lastRefillTicks = Environment.TickCount64;
        }

        public bool TryAcquire()
        {
            long now = Environment.TickCount64;
            double elapsed = (now - _lastRefillTicks) / 1000.0;
            _tokens = Math.Min(_capacity, _tokens + elapsed * _rate);
            _lastRefillTicks = now;

            if (_tokens >= 1.0)
            {
                _tokens -= 1.0;
                return true;
            }
            return false;
        }
    }
}
