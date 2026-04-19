package site.aster.trust;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import site.aster.registry.ServiceSummary;

/**
 * Wire types for the {@code aster.consumer_admission} ALPN (Aster-SPEC §3.2).
 *
 * <p>Port of {@code bindings/python/aster/trust/consumer.py} lines 43-133. Field names and shape
 * are normative; any divergence breaks Python↔Java admission interop.
 */
public final class ConsumerAdmissionWire {

  static final ObjectMapper MAPPER =
      new ObjectMapper().configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

  private ConsumerAdmissionWire() {}

  /**
   * Request sent by a consumer over the admission ALPN. {@code credentialJson} is the consumer's
   * enrollment credential serialized as JSON (empty string in dev/open-gate mode). {@code iidToken}
   * is optional (empty string when absent).
   */
  public static final class Request {
    @JsonProperty("credential_json")
    public String credentialJson = "";

    @JsonProperty("iid_token")
    public String iidToken = "";

    public static Request fromJson(byte[] bytes) {
      try {
        return MAPPER.readValue(bytes, Request.class);
      } catch (Exception e) {
        throw new IllegalArgumentException(
            "ConsumerAdmissionRequest JSON parse failed: " + e.getMessage(), e);
      }
    }

    /** Build a request with the given inner credential JSON (empty string = open-gate). */
    public static Request of(String credentialJson, String iidToken) {
      Request r = new Request();
      r.credentialJson = credentialJson == null ? "" : credentialJson;
      r.iidToken = iidToken == null ? "" : iidToken;
      return r;
    }

    public byte[] toJsonBytes() {
      try {
        return MAPPER.writeValueAsBytes(this);
      } catch (JsonProcessingException e) {
        throw new IllegalStateException("ConsumerAdmissionRequest serialization failed", e);
      }
    }
  }

  /** Parse a {@link Response} from its wire-format JSON bytes. */
  public static Response parseResponse(byte[] bytes) {
    try {
      return MAPPER.readValue(bytes, Response.class);
    } catch (Exception e) {
      throw new IllegalArgumentException(
          "ConsumerAdmissionResponse JSON parse failed: " + e.getMessage(), e);
    }
  }

  /**
   * Server reply after admission. {@code reason} MUST be empty in the wire response (spec §3.2.2,
   * oracle protection). {@code gossipTopic} is hex-encoded and included only when non-empty.
   */
  @JsonInclude(JsonInclude.Include.ALWAYS)
  public static final class Response {
    @JsonProperty("admitted")
    public boolean admitted;

    @JsonProperty("attributes")
    public Map<String, String> attributes = new LinkedHashMap<>();

    @JsonProperty("services")
    public List<ServiceSummary> services = new ArrayList<>();

    @JsonProperty("registry_namespace")
    public String registryNamespace = "";

    @JsonProperty("root_pubkey")
    public String rootPubkey = "";

    @JsonProperty("reason")
    public String reason = "";

    @JsonInclude(JsonInclude.Include.NON_EMPTY)
    @JsonProperty("gossip_topic")
    public String gossipTopic = "";

    public byte[] toJsonBytes() {
      try {
        return MAPPER.writeValueAsBytes(this);
      } catch (JsonProcessingException e) {
        throw new IllegalStateException("ConsumerAdmissionResponse serialization failed", e);
      }
    }

    public static Response admitted(List<ServiceSummary> services) {
      return admitted(services, "");
    }

    /**
     * Dev-mode admit with a registry namespace. {@code registryNamespace} is the 64-char hex doc-id
     * of the producer's registry doc so the consumer can {@code join_and_subscribe_namespace} and
     * fetch contract manifests.
     */
    public static Response admitted(List<ServiceSummary> services, String registryNamespace) {
      Response r = new Response();
      r.admitted = true;
      r.services = services;
      r.registryNamespace = registryNamespace == null ? "" : registryNamespace;
      return r;
    }

    public static Response denied() {
      Response r = new Response();
      r.admitted = false;
      return r;
    }
  }
}
