package site.aster.registry;

import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * Key-schema helpers for the Aster service registry.
 *
 * <p>All keys are UTF-8 encoded and suitable for iroh-docs set_bytes/query calls. See Aster-SPEC.md
 * §11.2 and §12.4 for the normative prefixes.
 */
public final class RegistryKeys {

  /** Registry download-policy prefixes -- all key namespaces a registry client should sync. */
  public static final List<byte[]> REGISTRY_PREFIXES =
      List.of(
          "contracts/".getBytes(StandardCharsets.UTF_8),
          "services/".getBytes(StandardCharsets.UTF_8),
          "endpoints/".getBytes(StandardCharsets.UTF_8),
          "compatibility/".getBytes(StandardCharsets.UTF_8),
          "_aster/".getBytes(StandardCharsets.UTF_8));

  private RegistryKeys() {}

  public static byte[] contractKey(String contractId) {
    return ("contracts/" + contractId).getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] versionKey(String name, int version) {
    return ("services/" + name + "/versions/v" + version).getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] channelKey(String name, String channel) {
    return ("services/" + name + "/channels/" + channel).getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] tagKey(String name, String tag) {
    return ("services/" + name + "/tags/" + tag).getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] leaseKey(String name, String contractId, String endpointId) {
    return ("services/" + name + "/contracts/" + contractId + "/endpoints/" + endpointId)
        .getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] leasePrefix(String name, String contractId) {
    return ("services/" + name + "/contracts/" + contractId + "/endpoints/")
        .getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] aclKey(String subkey) {
    return ("_aster/acl/" + subkey).getBytes(StandardCharsets.UTF_8);
  }

  public static byte[] configKey(String subkey) {
    return ("_aster/config/" + subkey).getBytes(StandardCharsets.UTF_8);
  }
}
