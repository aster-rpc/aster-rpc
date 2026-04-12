namespace Aster.Interceptors;

/// <summary>
/// Injects and/or validates auth metadata.
/// Token provider can be a static string or a delegate that returns a token.
/// Validator can be a static expected token or a delegate that validates.
/// </summary>
public class AuthInterceptor : IInterceptor
{
    private readonly object? _tokenProvider; // string | Func<string> | null
    private readonly object? _validator;     // string | Func<string, bool> | null
    private readonly string _metadataKey;
    private readonly string? _scheme;

    /// <param name="tokenProvider">A static token string, or a Func&lt;string&gt; that produces one.</param>
    /// <param name="validator">A static expected token string, or a Func&lt;string, bool&gt; predicate.</param>
    /// <param name="metadataKey">Metadata key to inject/read. Defaults to "authorization".</param>
    /// <param name="scheme">Auth scheme prefix (e.g. "Bearer"). Set to null to omit.</param>
    public AuthInterceptor(
        object? tokenProvider = null,
        object? validator = null,
        string metadataKey = "authorization",
        string? scheme = "Bearer")
    {
        _tokenProvider = tokenProvider;
        _validator = validator;
        _metadataKey = metadataKey;
        _scheme = scheme;
    }

    public object OnRequest(CallContext ctx, object request)
    {
        // Inject token if provider is set and key not already present
        if (_tokenProvider is not null && !ctx.Metadata.ContainsKey(_metadataKey))
        {
            string token = _tokenProvider switch
            {
                Func<string> provider => provider(),
                string staticToken => staticToken,
                _ => _tokenProvider.ToString()!
            };

            ctx.Metadata[_metadataKey] = _scheme is not null
                ? $"{_scheme} {token}"
                : token;
        }

        // Validate token if validator is set
        if (_validator is not null)
        {
            ctx.Metadata.TryGetValue(_metadataKey, out var rawToken);
            rawToken ??= "";
            var token = rawToken;

            var prefix = _scheme is not null ? $"{_scheme} " : "";
            if (prefix.Length > 0 && rawToken.StartsWith(prefix))
            {
                token = rawToken[prefix.Length..];
            }

            bool valid = _validator switch
            {
                Func<string, bool> predicate => predicate(token),
                string expected => token == expected,
                _ => false
            };

            if (!valid)
            {
                throw new RpcError(StatusCode.Unauthenticated, "authentication failed");
            }
        }

        return request;
    }
}
