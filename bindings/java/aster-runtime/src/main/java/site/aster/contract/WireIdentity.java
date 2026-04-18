package site.aster.contract;

import site.aster.annotations.WireType;

/**
 * Language-neutral identity of a wire type — the {@code (package, name)} pair used as a graph key
 * and as the {@code self_ref_name} in a FieldDef's SELF_REF. Must match whatever the peer binding
 * (Python, TS, etc.) emits for the same logical type, or {@code contract_id} diverges.
 *
 * <p>Derivation rules, in order:
 *
 * <ol>
 *   <li>If the class carries {@code @WireType("ns/Name")}: split on the last slash; {@code package
 *       = "ns"}, {@code name = "Name"}. A tag without a slash is treated as {@code package = ""},
 *       {@code name = tag}.
 *   <li>Else fall back to Java's package + simple name.
 * </ol>
 *
 * <p>Mirrors Python's {@code _get_package_name} / {@code _get_type_name} from {@code
 * bindings/python/aster/contract/identity.py}.
 */
public record WireIdentity(String packageName, String name) {

  public WireIdentity {
    packageName = packageName == null ? "" : packageName;
    name = name == null ? "" : name;
  }

  /**
   * Canonical FQN string used as the type-graph key. Matches Python's {@code _get_fqn(cls)}: {@code
   * "<package>.<name>"} when package is non-empty, bare {@code name} otherwise.
   */
  public String fqn() {
    return packageName.isEmpty() ? name : packageName + "." + name;
  }

  public static WireIdentity of(Class<?> cls) {
    WireType ann = cls.getAnnotation(WireType.class);
    if (ann != null) {
      String tag = ann.value();
      int slash = tag.lastIndexOf('/');
      if (slash < 0) {
        return new WireIdentity("", tag);
      }
      return new WireIdentity(tag.substring(0, slash), tag.substring(slash + 1));
    }
    Package pkg = cls.getPackage();
    String pkgName = pkg == null ? "" : pkg.getName();
    return new WireIdentity(pkgName, cls.getSimpleName());
  }
}
