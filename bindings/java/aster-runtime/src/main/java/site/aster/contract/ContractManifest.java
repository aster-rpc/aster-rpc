package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;
import java.util.List;
import java.util.Map;

/**
 * Published-side persisted record of a service contract's canonical identity. Mirrors Python's
 * {@code aster.contract.manifest.ContractManifest} (spec §11.4); the JSON shape is the forever-
 * interchange format between producers and consumers, so every field name / type here is pinned to
 * the Python reference.
 *
 * <p>The {@code contract_id} and {@code type_hashes} fields are canonical (produced by hashing the
 * {@link ServiceContract} / {@link TypeDef} canonical bytes in Rust). The rest — {@code
 * description}, {@code tags}, {@code methods[*].fields}, etc. — are non-canonical and round-trip
 * through the manifest without affecting the hash.
 */
@JsonPropertyOrder({
  "v",
  "service",
  "version",
  "contract_id",
  "canonical_encoding",
  "type_count",
  "type_hashes",
  "method_count",
  "methods",
  "serialization_modes",
  "producer_language",
  "scoped",
  "description",
  "tags",
  "deprecated",
  "semver",
  "vcs_revision",
  "vcs_tag",
  "vcs_url",
  "changelog",
  "published_by",
  "published_at_epoch_ms"
})
@JsonInclude(JsonInclude.Include.ALWAYS)
public record ContractManifest(
    @JsonProperty("v") int v,
    @JsonProperty("service") String service,
    @JsonProperty("version") int version,
    @JsonProperty("contract_id") String contractId,
    @JsonProperty("canonical_encoding") String canonicalEncoding,
    @JsonProperty("type_count") int typeCount,
    @JsonProperty("type_hashes") List<String> typeHashes,
    @JsonProperty("method_count") int methodCount,
    @JsonProperty("methods") List<Map<String, Object>> methods,
    @JsonProperty("serialization_modes") List<String> serializationModes,
    @JsonProperty("producer_language") String producerLanguage,
    @JsonProperty("scoped") String scoped,
    @JsonProperty("description") String description,
    @JsonProperty("tags") List<String> tags,
    @JsonProperty("deprecated") boolean deprecated,
    @JsonProperty("semver") String semver,
    @JsonProperty("vcs_revision") String vcsRevision,
    @JsonProperty("vcs_tag") String vcsTag,
    @JsonProperty("vcs_url") String vcsUrl,
    @JsonProperty("changelog") String changelog,
    @JsonProperty("published_by") String publishedBy,
    @JsonProperty("published_at_epoch_ms") long publishedAtEpochMs) {

  public static final int FIELD_SCHEMA_VERSION = 1;

  public ContractManifest {
    service = service == null ? "" : service;
    contractId = contractId == null ? "" : contractId;
    canonicalEncoding = canonicalEncoding == null ? "fory-xlang/0.15" : canonicalEncoding;
    typeHashes = typeHashes == null ? List.of() : List.copyOf(typeHashes);
    methods = methods == null ? List.of() : List.copyOf(methods);
    serializationModes = serializationModes == null ? List.of() : List.copyOf(serializationModes);
    producerLanguage = producerLanguage == null ? "" : producerLanguage;
    scoped = scoped == null ? "shared" : scoped;
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
    publishedBy = publishedBy == null ? "" : publishedBy;
  }

  @JsonCreator
  public static ContractManifest deserialize(
      @JsonProperty("v") Integer v,
      @JsonProperty("service") String service,
      @JsonProperty("version") Integer version,
      @JsonProperty("contract_id") String contractId,
      @JsonProperty("canonical_encoding") String canonicalEncoding,
      @JsonProperty("type_count") Integer typeCount,
      @JsonProperty("type_hashes") List<String> typeHashes,
      @JsonProperty("method_count") Integer methodCount,
      @JsonProperty("methods") List<Map<String, Object>> methods,
      @JsonProperty("serialization_modes") List<String> serializationModes,
      @JsonProperty("producer_language") String producerLanguage,
      @JsonProperty("scoped") String scoped,
      @JsonProperty("description") String description,
      @JsonProperty("tags") List<String> tags,
      @JsonProperty("deprecated") Boolean deprecated,
      @JsonProperty("semver") String semver,
      @JsonProperty("vcs_revision") String vcsRevision,
      @JsonProperty("vcs_tag") String vcsTag,
      @JsonProperty("vcs_url") String vcsUrl,
      @JsonProperty("changelog") String changelog,
      @JsonProperty("published_by") String publishedBy,
      @JsonProperty("published_at_epoch_ms") Long publishedAtEpochMs) {
    return new ContractManifest(
        v == null ? FIELD_SCHEMA_VERSION : v,
        service,
        version == null ? 0 : version,
        contractId,
        canonicalEncoding,
        typeCount == null ? 0 : typeCount,
        typeHashes,
        methodCount == null ? 0 : methodCount,
        methods,
        serializationModes,
        producerLanguage,
        scoped,
        description,
        tags,
        deprecated != null && deprecated,
        semver,
        vcsRevision,
        vcsTag,
        vcsUrl,
        changelog,
        publishedBy,
        publishedAtEpochMs == null ? 0L : publishedAtEpochMs);
  }

  public String toJson() {
    return ContractJson.toJson(this);
  }
}
