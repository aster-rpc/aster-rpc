package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;

/**
 * One RPC method in a {@link ServiceContract}. Request / response type references are hex-encoded
 * 32-byte BLAKE3 digests of their corresponding {@link TypeDef} canonical bytes.
 */
@JsonPropertyOrder({
  "name",
  "pattern",
  "request_type",
  "response_type",
  "idempotent",
  "default_timeout",
  "requires"
})
@JsonInclude(JsonInclude.Include.ALWAYS)
public record MethodDef(
    @JsonProperty("name") String name,
    @JsonProperty("pattern") MethodPattern pattern,
    @JsonProperty("request_type") String requestType,
    @JsonProperty("response_type") String responseType,
    @JsonProperty("idempotent") boolean idempotent,
    @JsonProperty("default_timeout") double defaultTimeout,
    @JsonProperty("requires") CapabilityRequirement requires) {

  public MethodDef {
    name = name == null ? "" : name;
    pattern = pattern == null ? MethodPattern.UNARY : pattern;
    requestType = requestType == null ? "" : requestType;
    responseType = responseType == null ? "" : responseType;
  }

  @JsonCreator
  public static MethodDef deserialize(
      @JsonProperty("name") String name,
      @JsonProperty("pattern") MethodPattern pattern,
      @JsonProperty("request_type") String requestType,
      @JsonProperty("response_type") String responseType,
      @JsonProperty("idempotent") Boolean idempotent,
      @JsonProperty("default_timeout") Double defaultTimeout,
      @JsonProperty("requires") CapabilityRequirement requires) {
    return new MethodDef(
        name,
        pattern,
        requestType,
        responseType,
        idempotent != null && idempotent,
        defaultTimeout == null ? 0.0 : defaultTimeout,
        requires);
  }
}
