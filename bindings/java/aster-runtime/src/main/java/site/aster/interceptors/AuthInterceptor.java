package site.aster.interceptors;

import java.util.function.Predicate;
import java.util.function.Supplier;

/**
 * Authentication interceptor that injects and/or validates auth metadata.
 *
 * <p>On the client side, supply a {@code tokenProvider} to automatically inject an authorization
 * header. On the server side, supply a {@code validator} to verify the token on each request.
 */
public final class AuthInterceptor implements Interceptor {

  private final Supplier<String> tokenProvider;
  private final Predicate<String> validator;
  private final String metadataKey;
  private final String scheme;

  private AuthInterceptor(Builder builder) {
    this.tokenProvider = builder.tokenProvider;
    this.validator = builder.validator;
    this.metadataKey = builder.metadataKey;
    this.scheme = builder.scheme;
  }

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    // Inject token if provider is set and metadata key is absent
    if (tokenProvider != null && !ctx.metadata().containsKey(metadataKey)) {
      String token = tokenProvider.get();
      String headerValue = scheme != null ? scheme + " " + token : token;
      ctx.metadata().put(metadataKey, headerValue);
    }

    // Validate token if validator is set
    if (validator != null) {
      String rawToken = ctx.metadata().getOrDefault(metadataKey, "");
      String token = rawToken;
      if (scheme != null) {
        String prefix = scheme + " ";
        if (rawToken.startsWith(prefix)) {
          token = rawToken.substring(prefix.length());
        }
      }
      if (!validator.test(token)) {
        throw new RpcError(StatusCode.UNAUTHENTICATED, "authentication failed");
      }
    }

    return request;
  }

  public static Builder builder() {
    return new Builder();
  }

  public static final class Builder {
    private Supplier<String> tokenProvider;
    private Predicate<String> validator;
    private String metadataKey = "authorization";
    private String scheme = "Bearer";

    private Builder() {}

    /** Sets a static token to inject into request metadata. */
    public Builder tokenProvider(String token) {
      this.tokenProvider = () -> token;
      return this;
    }

    /** Sets a dynamic token supplier to inject into request metadata. */
    public Builder tokenProvider(Supplier<String> tokenProvider) {
      this.tokenProvider = tokenProvider;
      return this;
    }

    /** Sets a static expected token for validation. */
    public Builder validator(String expectedToken) {
      this.validator = t -> t.equals(expectedToken);
      return this;
    }

    /** Sets a predicate for token validation. */
    public Builder validator(Predicate<String> validator) {
      this.validator = validator;
      return this;
    }

    /** Sets the metadata key used for the authorization header. Default: "authorization". */
    public Builder metadataKey(String metadataKey) {
      this.metadataKey = metadataKey;
      return this;
    }

    /** Sets the scheme prefix (e.g. "Bearer"). Set to {@code null} for no prefix. */
    public Builder scheme(String scheme) {
      this.scheme = scheme;
      return this;
    }

    public AuthInterceptor build() {
      return new AuthInterceptor(this);
    }
  }
}
