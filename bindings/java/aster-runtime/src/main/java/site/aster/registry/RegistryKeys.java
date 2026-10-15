package site.aster.registry;

import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * Key-schema helpers for the Aster service registry.
 *
 * <p>All keys are UTF-8 encoded and suitable for iroh-docs set_bytes/query calls, identical to
 * {@code core/src/registry.rs}. See Aster-SPEC.md §11.2 and §12.4.
 *
 * <p>The keys are prefix-free: no key is a byte prefix of another. iroh-docs lets a write at a key
 * (a value or a deletion marker) supersede the same author's older entries whose keys extend it, so
 * with {@code .../versions/v1} a prefix of {@code .../versions/v10}, publishing or retracting v1
 * would remove v10. Every segment ends in {@code /} and variable segments escape {@code %} and
 * {@code /} ({@code %25}, {@code %2F}), so a segment never holds a raw {@code /} and no family's
 * key is an interior path of another.
 */
public final class RegistryKeys {

  /**
   * Registry download-policy prefixes -- all key namespaces a registry client should sync. Every
   * family a builder below writes must be covered, or the entry syncs without its value; {@code
   * endpoints/} and {@code compatibility/} are spec-reserved families with no builder yet.
   */
  public static final List<byte[]> REGISTRY_PREFIXES =
      List.of(
          "contracts/".getBytes(StandardCharsets.UTF_8),
          "manifests/".getBytes(StandardCharsets.UTF_8),
          "services/".getBytes(StandardCharsets.UTF_8),
          "endpoints/".getBytes(StandardCharsets.UTF_8),
          "compatibility/".getBytes(StandardCharsets.UTF_8),
          "_aster/".getBytes(StandardCharsets.UTF_8));

  private RegistryKeys() {}

  /** Escape a variable key segment: {@code %} to {@code %25}, {@code /} to {@code %2F}. */
  public static String keySegment(String segment) {
    return segment.replace("%", "%25").replace("/", "%2F");
  }

  private static byte[] utf8(String key) {
    return key.getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] contractKey(String contractId) {
    return utf8("contracts/" + keySegment(contractId) + "/");
  }

  /** The inline manifest shortcut: {@code manifests/{contract_id}/}. */
  public static byte[] manifestKey(String contractId) {
    return utf8("manifests/" + keySegment(contractId) + "/");
  }

  public static byte[] versionKey(String name, int version) {
    return utf8("services/" + keySegment(name) + "/versions/v" + version + "/");
  }

  /** Every version pointer of one service, and nothing else. */
  public static byte[] versionPrefix(String name) {
    return utf8("services/" + keySegment(name) + "/versions/");
  }

  public static byte[] channelKey(String name, String channel) {
    return utf8("services/" + keySegment(name) + "/channels/" + keySegment(channel) + "/");
  }

  public static byte[] tagKey(String name, String tag) {
    return utf8("services/" + keySegment(name) + "/tags/" + keySegment(tag) + "/");
  }

  public static byte[] leaseKey(String name, String contractId, String endpointId) {
    return utf8(
        "services/"
            + keySegment(name)
            + "/contracts/"
            + keySegment(contractId)
            + "/endpoints/"
            + keySegment(endpointId)
            + "/");
  }

  public static byte[] leasePrefix(String name, String contractId) {
    return utf8(
        "services/" + keySegment(name) + "/contracts/" + keySegment(contractId) + "/endpoints/");
  }

  public static byte[] aclKey(String subkey) {
    return utf8("_aster/acl/" + keySegment(subkey) + "/");
  }

  public static byte[] configKey(String subkey) {
    return utf8("_aster/config/" + keySegment(subkey) + "/");
  }
}
