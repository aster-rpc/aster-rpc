package site.aster.config;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import org.tomlj.Toml;
import org.tomlj.TomlArray;
import org.tomlj.TomlParseResult;
import org.tomlj.TomlTable;

/**
 * Parsed {@code .aster-identity} TOML file. Mirrors the Python loader in {@code
 * bindings/python/aster/config.py} ({@code load_identity}/{@code load_identity_from_path}): a
 * node-level secret key plus a list of signed peer entries.
 *
 * <p>The peer entries carry everything needed to construct a consumer-admission credential: root
 * pubkey, endpoint id, expiry, signature, and attributes. {@link #credentialJson(PeerEntry)}
 * serialises a peer entry to the wire JSON accepted by the {@code aster.consumer_admission} ALPN
 * handler — byte-identical to Python's {@code consumer_cred_to_json}.
 */
public final class AsterIdentity {

  private static final ObjectMapper MAPPER = new ObjectMapper();

  private final byte[] nodeSecretKey;
  private final String nodeEndpointId;
  private final List<PeerEntry> peers;

  private AsterIdentity(byte[] nodeSecretKey, String nodeEndpointId, List<PeerEntry> peers) {
    this.nodeSecretKey = nodeSecretKey;
    this.nodeEndpointId = nodeEndpointId;
    this.peers = List.copyOf(peers);
  }

  /** 32-byte ed25519 secret key for the node, or {@code null} if the file has no {@code [node]}. */
  public byte[] nodeSecretKey() {
    return nodeSecretKey;
  }

  /** Hex-encoded endpoint id matching {@link #nodeSecretKey()}, or empty when absent. */
  public String nodeEndpointId() {
    return nodeEndpointId;
  }

  /** All {@code [[peers]]} entries from the file, in source order. */
  public List<PeerEntry> peers() {
    return peers;
  }

  /** First peer matching {@code name}, or empty. */
  public Optional<PeerEntry> findByName(String name) {
    if (name == null) return Optional.empty();
    for (PeerEntry p : peers) {
      if (name.equals(p.name)) return Optional.of(p);
    }
    return Optional.empty();
  }

  /** First peer matching {@code role} (e.g. {@code "consumer"}), or empty. */
  public Optional<PeerEntry> findByRole(String role) {
    if (role == null) return Optional.empty();
    for (PeerEntry p : peers) {
      if (role.equals(p.role)) return Optional.of(p);
    }
    return Optional.empty();
  }

  /**
   * Load an {@code .aster-identity} TOML file. Matches the format produced by {@code aster enroll
   * node}:
   *
   * <pre>
   *   [node]
   *   secret_key = "&lt;base64 32 bytes&gt;"
   *   endpoint_id = "&lt;hex 64&gt;"
   *
   *   [[peers]]
   *   name = "edge-node-7"
   *   role = "consumer"
   *   type = "policy"
   *   root_pubkey = "&lt;hex 64&gt;"
   *   endpoint_id = "&lt;hex 64&gt;"
   *   expires_at = 1779170133
   *   signature = "&lt;hex 128&gt;"
   *   attributes = { "aster.role" = "ops.status" }
   * </pre>
   */
  public static AsterIdentity load(Path path) {
    TomlParseResult toml;
    try {
      toml = Toml.parse(path);
    } catch (Exception e) {
      throw new IllegalArgumentException(
          "failed to parse .aster-identity TOML at " + path + ": " + e.getMessage(), e);
    }
    if (toml.hasErrors()) {
      throw new IllegalArgumentException(
          ".aster-identity TOML parse errors at " + path + ": " + toml.errors());
    }

    byte[] secretKey = null;
    String endpointId = "";
    TomlTable nodeTbl = toml.getTable("node");
    if (nodeTbl != null) {
      String b64 = nodeTbl.getString("secret_key");
      if (b64 != null && !b64.isEmpty()) {
        secretKey = Base64.getDecoder().decode(b64);
      }
      String eid = nodeTbl.getString("endpoint_id");
      if (eid != null) endpointId = eid;
    }

    List<PeerEntry> peerList = new ArrayList<>();
    TomlArray peerArr = toml.getArray("peers");
    if (peerArr != null) {
      for (int i = 0; i < peerArr.size(); i++) {
        TomlTable t = peerArr.getTable(i);
        peerList.add(peerFromToml(t));
      }
    }

    return new AsterIdentity(secretKey, endpointId, peerList);
  }

  public static AsterIdentity load(String path) {
    return load(Path.of(path));
  }

  private static PeerEntry peerFromToml(TomlTable t) {
    Map<String, String> attrs = new LinkedHashMap<>();
    TomlTable attrsTbl = t.getTable("attributes");
    if (attrsTbl != null) {
      // entrySet() is the only tomlj accessor that round-trips keys with embedded dots
      // ("aster.role") cleanly. `keySet()` returns unquoted names but `get()` then requires
      // the quoted form back, so iteration + get() silently drops such entries.
      for (Map.Entry<String, Object> e : attrsTbl.entrySet()) {
        if (e.getValue() != null) {
          attrs.put(e.getKey(), e.getValue().toString());
        }
      }
    }
    return new PeerEntry(
        orEmpty(t.getString("name")),
        orEmpty(t.getString("role")),
        orElse(t.getString("type"), "policy"),
        orEmpty(t.getString("root_pubkey")),
        orEmpty(t.getString("endpoint_id")),
        longOr(t, "expires_at", 0L),
        orEmpty(t.getString("nonce")),
        orEmpty(t.getString("signature")),
        Map.copyOf(attrs));
  }

  /**
   * Serialise a peer entry to the JSON wire format used in {@code
   * ConsumerAdmissionRequest.credential_json}. Byte-identical to Python's {@code
   * consumer_cred_to_json(_credential_from_peer_entry(peer))}.
   */
  public static String credentialJson(PeerEntry peer) {
    Map<String, Object> out = new LinkedHashMap<>();
    out.put("credential_type", peer.credentialType);
    out.put("root_pubkey", peer.rootPubkey);
    out.put("expires_at", peer.expiresAt);
    out.put("attributes", peer.attributes);
    out.put("endpoint_id", peer.endpointId.isEmpty() ? null : peer.endpointId);
    out.put("nonce", peer.nonce.isEmpty() ? null : peer.nonce);
    out.put("signature", peer.signature);
    try {
      return MAPPER.writeValueAsString(out);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException("failed to serialise peer credential JSON", e);
    }
  }

  // ─── PeerEntry ──────────────────────────────────────────────────────────

  /**
   * One {@code [[peers]]} entry. All hex/b64 fields stay in their encoded string form so round-trip
   * to the wire JSON is lossless.
   */
  public record PeerEntry(
      String name,
      String role,
      String credentialType,
      String rootPubkey,
      String endpointId,
      long expiresAt,
      String nonce,
      String signature,
      Map<String, String> attributes) {}

  // ─── helpers ────────────────────────────────────────────────────────────

  private static String orEmpty(String s) {
    return s == null ? "" : s;
  }

  private static String orElse(String s, String fallback) {
    return s == null || s.isEmpty() ? fallback : s;
  }

  private static long longOr(TomlTable t, String key, long fallback) {
    Long v = t.getLong(key);
    return v == null ? fallback : v;
  }
}
