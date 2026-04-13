package com.aster.registry;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.core.JsonProcessingException;
import java.util.ArrayList;
import java.util.List;

/**
 * Renewable advertisement for a live endpoint (Aster-SPEC.md §11.6).
 *
 * <p>Stored at {@code services/{name}/contracts/{cid}/endpoints/{eid}}.
 */
@JsonInclude(JsonInclude.Include.ALWAYS)
public final class EndpointLease {

  @JsonProperty("endpoint_id")
  public String endpointId = "";

  @JsonProperty("contract_id")
  public String contractId = "";

  @JsonProperty("service")
  public String service = "";

  @JsonProperty("version")
  public int version;

  @JsonProperty("lease_expires_epoch_ms")
  public long leaseExpiresEpochMs;

  @JsonProperty("lease_seq")
  public long leaseSeq;

  @JsonProperty("alpn")
  public String alpn = "aster/1";

  @JsonProperty("serialization_modes")
  public List<String> serializationModes = new ArrayList<>();

  @JsonProperty("feature_flags")
  public List<String> featureFlags = new ArrayList<>();

  @JsonProperty("relay_url")
  public String relayUrl;

  @JsonProperty("direct_addrs")
  public List<String> directAddrs = new ArrayList<>();

  @JsonProperty("load")
  public Double load;

  @JsonProperty("language_runtime")
  public String languageRuntime;

  @JsonProperty("aster_version")
  public String asterVersion = "";

  @JsonProperty("policy_realm")
  public String policyRealm;

  @JsonProperty("health_status")
  public String healthStatus = HealthStatus.STARTING;

  @JsonProperty("tags")
  public List<String> tags = new ArrayList<>();

  @JsonProperty("updated_at_epoch_ms")
  public long updatedAtEpochMs;

  /** Return true if this lease has not expired. */
  public boolean isFresh(int leaseDurationS) {
    long nowMs = System.currentTimeMillis();
    return (nowMs - updatedAtEpochMs) <= leaseDurationS * 1000L;
  }

  /** Return true if health is READY or DEGRADED (not STARTING/DRAINING). */
  public boolean isRoutable() {
    return HealthStatus.isRoutable(healthStatus);
  }

  public String toJson() {
    try {
      return RegistryMapper.MAPPER.writeValueAsString(this);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException("EndpointLease serialization failed", e);
    }
  }

  public static EndpointLease fromJson(String json) {
    try {
      return RegistryMapper.MAPPER.readValue(json, EndpointLease.class);
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("EndpointLease deserialization failed", e);
    }
  }
}
