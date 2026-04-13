package site.aster.registry;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.core.JsonProcessingException;

/**
 * Docs pointer to an immutable Iroh collection (Aster-SPEC.md §11.2.1).
 *
 * <p>Stored at {@code contracts/{contract_id}} in the registry doc. {@code collectionFormat} is
 * {@code "raw"} for single-blob (Phase 10 default) or {@code "index"} for multi-file collections.
 * Old records without the field default to {@code "raw"} on deserialization.
 */
@JsonInclude(JsonInclude.Include.ALWAYS)
public final class ArtifactRef {

  @JsonProperty("contract_id")
  public String contractId = "";

  @JsonProperty("collection_hash")
  public String collectionHash = "";

  @JsonProperty("provider_endpoint_id")
  public String providerEndpointId;

  @JsonProperty("relay_url")
  public String relayUrl;

  @JsonProperty("ticket")
  public String ticket;

  @JsonProperty("published_by")
  public String publishedBy = "";

  @JsonProperty("published_at_epoch_ms")
  public long publishedAtEpochMs;

  @JsonProperty("collection_format")
  public String collectionFormat = "raw";

  public String toJson() {
    try {
      return RegistryMapper.MAPPER.writeValueAsString(this);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException("ArtifactRef serialization failed", e);
    }
  }

  public static ArtifactRef fromJson(String json) {
    try {
      ArtifactRef r = RegistryMapper.MAPPER.readValue(json, ArtifactRef.class);
      if (r.collectionFormat == null || r.collectionFormat.isEmpty()) {
        r.collectionFormat = "raw";
      }
      return r;
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("ArtifactRef deserialization failed", e);
    }
  }
}
