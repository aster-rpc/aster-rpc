package site.aster.registry;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonValue;
import com.fasterxml.jackson.core.JsonProcessingException;

/** Flat change notification broadcast over gossip (Aster-SPEC.md §11.7). */
@JsonInclude(JsonInclude.Include.ALWAYS)
public final class GossipEvent {

  @JsonProperty("type")
  public Kind type;

  @JsonProperty("service")
  public String service;

  @JsonProperty("version")
  public Integer version;

  @JsonProperty("channel")
  public String channel;

  @JsonProperty("contract_id")
  public String contractId;

  @JsonProperty("endpoint_id")
  public String endpointId;

  @JsonProperty("key_prefix")
  public String keyPrefix;

  @JsonProperty("timestamp_ms")
  public long timestampMs;

  public String toJson() {
    try {
      return RegistryMapper.MAPPER.writeValueAsString(this);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException("GossipEvent serialization failed", e);
    }
  }

  public static GossipEvent fromJson(String json) {
    try {
      return RegistryMapper.MAPPER.readValue(json, GossipEvent.class);
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("GossipEvent deserialization failed", e);
    }
  }

  /**
   * Jackson-friendly wrapper over {@link GossipEventType} that serializes as an int code and
   * forwards to the canonical enum.
   */
  public static final class Kind {
    public final GossipEventType value;

    @JsonCreator
    public static Kind fromCode(int code) {
      return new Kind(GossipEventType.fromCode(code));
    }

    public Kind(GossipEventType value) {
      this.value = value;
    }

    @JsonValue
    public int code() {
      return value.code();
    }
  }
}
