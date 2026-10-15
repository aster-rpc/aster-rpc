package site.aster.registry;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * Registry keys: the documented format, prefix-free (iroh-docs lets a write at a key supersede the
 * same author's older entries whose keys extend it), and byte-identical to core's builders through
 * the C ABI.
 */
final class RegistryKeysTest {
  private static String s(byte[] key) {
    return new String(key, StandardCharsets.UTF_8);
  }

  @Test
  void format() {
    assertEquals("contracts/abc/", s(RegistryKeys.contractKey("abc")));
    assertEquals("manifests/abc/", s(RegistryKeys.manifestKey("abc")));
    assertEquals("services/svc/versions/v7/", s(RegistryKeys.versionKey("svc", 7)));
    assertEquals("services/svc/versions/", s(RegistryKeys.versionPrefix("svc")));
    assertEquals("services/svc/channels/stable/", s(RegistryKeys.channelKey("svc", "stable")));
    assertEquals("services/svc/tags/latest/", s(RegistryKeys.tagKey("svc", "latest")));
    assertEquals(
        "services/svc/contracts/cid/endpoints/eid/", s(RegistryKeys.leaseKey("svc", "cid", "eid")));
    assertEquals(
        "services/svc/contracts/cid/endpoints/", s(RegistryKeys.leasePrefix("svc", "cid")));
    assertEquals("_aster/acl/writers/", s(RegistryKeys.aclKey("writers")));
    assertEquals("_aster/config/version/", s(RegistryKeys.configKey("version")));
    assertEquals("services/x/channels/a%2Fb%25c/", s(RegistryKeys.channelKey("x", "a/b%c")));
  }

  @Test
  void prefixFree() {
    String[] leaves = {"1", "10", "stable", "stable2", "x/y", "x", "", "%2F"};
    List<String> keys = new ArrayList<>();
    for (String name : new String[] {"svc", "svc2", "a/b", "a", "50%"}) {
      for (String leaf : leaves) {
        keys.add(s(RegistryKeys.contractKey(leaf)));
        keys.add(s(RegistryKeys.manifestKey(leaf)));
        keys.add(s(RegistryKeys.aclKey(leaf)));
        keys.add(s(RegistryKeys.configKey(leaf)));
        keys.add(s(RegistryKeys.channelKey(name, leaf)));
        keys.add(s(RegistryKeys.tagKey(name, leaf)));
        for (String eid : leaves) {
          keys.add(s(RegistryKeys.leaseKey(name, leaf, eid)));
        }
      }
      for (int v : new int[] {1, 10, 100, -1}) {
        keys.add(s(RegistryKeys.versionKey(name, v)));
      }
    }
    for (String a : keys) {
      for (String b : keys) {
        assertFalse(!a.equals(b) && b.startsWith(a), a + " is a prefix of " + b);
      }
    }
  }

  @Test
  void matchesRust() {
    assertArrayEquals(Registry.contractKeyRust("abc"), RegistryKeys.contractKey("abc"));
    assertArrayEquals(Registry.versionKeyRust("a/b", 10), RegistryKeys.versionKey("a/b", 10));
    assertArrayEquals(
        Registry.channelKeyRust("svc", "st/able"), RegistryKeys.channelKey("svc", "st/able"));
    assertArrayEquals(
        Registry.leaseKeyRust("svc", "cid", "e%id"), RegistryKeys.leaseKey("svc", "cid", "e%id"));
    assertArrayEquals(
        Registry.leasePrefixRust("svc", "cid"), RegistryKeys.leasePrefix("svc", "cid"));
    assertArrayEquals(Registry.aclKeyRust("writers"), RegistryKeys.aclKey("writers"));
  }
}
