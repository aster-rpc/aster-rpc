package site.aster.contract;

import java.lang.reflect.ParameterizedType;
import java.lang.reflect.RecordComponent;
import java.lang.reflect.Type;
import java.text.Normalizer;
import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import site.aster.server.spi.FieldMetadata;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.MethodMetadata;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;

/**
 * Assembles a {@link ContractManifest} from a live {@link ServiceDispatcher}. End-to-end flow:
 *
 * <ol>
 *   <li>Collect request / response classes from the dispatcher SPI.
 *   <li>Walk the type graph with {@link TypeGraphWalker}.
 *   <li>Resolve cycles via Tarjan SCC + build TypeDef JSONs with real type_hashes ({@link
 *       ContractIdentityResolver}).
 *   <li>Build {@link ServiceContract} JSON, sort MethodDefs by NFC name, compute contract_id via
 *       the Rust FFI.
 *   <li>Assemble the manifest: non-canonical description / tags / method field schemas pulled from
 *       dispatcher metadata + reflection.
 * </ol>
 *
 * <p>Mirrors {@code bindings/python/aster/contract/publication.build_collection} + {@code
 * aster.contract.manifest.extract_method_descriptors}.
 */
public final class ContractManifestBuilder {

  private ContractManifestBuilder() {}

  public static ContractManifest build(ServiceDispatcher dispatcher) {
    ServiceDescriptor sd = dispatcher.descriptor();

    Set<Class<?>> roots = new LinkedHashSet<>();
    roots.addAll(dispatcher.requestClasses().values());
    roots.addAll(dispatcher.responseClasses().values());
    // Inline param types may be primitive; the walker's eligibility filter drops them.
    for (MethodDispatcher mdisp : dispatcher.methods().values()) {
      for (var p : mdisp.descriptor().inlineParams()) {
        if (p.javaType() != null) {
          roots.add(p.javaType());
        }
      }
    }

    TypeGraphWalker.TypeGraph graph = TypeGraphWalker.walk(roots);
    ContractIdentityResolver.ResolvedTypes resolved = ContractIdentityResolver.resolve(graph);

    List<String> orderedMethodNames = new ArrayList<>(dispatcher.methods().keySet());
    orderedMethodNames.sort(ContractManifestBuilder::nfcCompare);

    List<MethodDef> methodDefs = new ArrayList<>();
    for (String methodName : orderedMethodNames) {
      MethodDispatcher md = dispatcher.methods().get(methodName);
      MethodDescriptor mdesc = md.descriptor();
      String reqHash = hashFor(dispatcher.requestClasses().get(methodName), resolved);
      String respHash = hashFor(dispatcher.responseClasses().get(methodName), resolved);
      methodDefs.add(
          new MethodDef(
              methodName,
              MethodPattern.fromStreamingKind(mdesc.streaming()),
              reqHash,
              respHash,
              mdesc.idempotent(),
              0.0,
              null));
    }

    ServiceContract sc =
        new ServiceContract(
            sd.name(),
            sd.version(),
            methodDefs,
            List.of("xlang"),
            ScopeKind.fromAnnotation(sd.scope()),
            null,
            "");
    String contractId = ContractIdentity.computeContractId(sc.toJson());

    // Manifest method dicts (v1 field schema + metadata).
    List<Map<String, Object>> methodDicts = new ArrayList<>();
    for (String methodName : orderedMethodNames) {
      methodDicts.add(buildMethodDict(dispatcher, methodName));
    }

    List<String> typeHashesSorted = new ArrayList<>(resolved.typeHashes().values());
    Collections.sort(typeHashesSorted);

    return new ContractManifest(
        ContractManifest.FIELD_SCHEMA_VERSION,
        sd.name(),
        sd.version(),
        contractId,
        "fory-xlang/0.15",
        resolved.typeDefs().size(),
        typeHashesSorted,
        methodDefs.size(),
        methodDicts,
        List.of("xlang"),
        "",
        ScopeKind.fromAnnotation(sd.scope()).wire(),
        dispatcher.description(),
        dispatcher.tags(),
        false,
        null,
        null,
        null,
        null,
        null,
        "",
        0L);
  }

  private static String hashFor(Class<?> cls, ContractIdentityResolver.ResolvedTypes resolved) {
    if (cls == null) {
      return "00".repeat(32);
    }
    String fqn = WireIdentity.of(cls).fqn();
    return resolved.typeHashes().getOrDefault(fqn, "00".repeat(32));
  }

  // ── Method dict (v1 field schema + rich metadata) ─────────────────────────

  private static Map<String, Object> buildMethodDict(
      ServiceDispatcher dispatcher, String methodName) {
    MethodDispatcher md = dispatcher.methods().get(methodName);
    MethodDescriptor mdesc = md.descriptor();
    MethodMetadata meta = dispatcher.methodMetadata(methodName);
    Class<?> reqCls = dispatcher.requestClasses().get(methodName);
    Class<?> respCls = dispatcher.responseClasses().get(methodName);

    Map<String, Object> out = new LinkedHashMap<>();
    out.put("name", methodName);
    out.put("pattern", MethodPattern.fromStreamingKind(mdesc.streaming()).wire());
    out.put("request_type", reqCls == null ? "" : reqCls.getSimpleName());
    out.put("response_type", respCls == null ? "" : respCls.getSimpleName());
    out.put("request_wire_tag", mdesc.requestType());
    out.put("response_wire_tag", mdesc.responseType());
    out.put("timeout", null);
    out.put("idempotent", mdesc.idempotent());
    out.put("has_context_param", mdesc.hasContextParam());
    out.put("fields", fieldsFor(reqCls, meta));
    out.put("response_fields", fieldsFor(respCls, MethodMetadata.EMPTY));
    out.put("request_style", mdesc.requestStyle() == RequestStyle.INLINE ? "inline" : "explicit");
    out.put("inline_params", inlineParamDicts(mdesc, meta));
    out.put("description", meta.description());
    out.put("tags", meta.tags());
    out.put("deprecated", meta.deprecated());
    return out;
  }

  private static List<Map<String, Object>> inlineParamDicts(
      MethodDescriptor mdesc, MethodMetadata meta) {
    if (mdesc.requestStyle() != RequestStyle.INLINE) {
      return List.of();
    }
    List<Map<String, Object>> out = new ArrayList<>();
    for (var p : mdesc.inlineParams()) {
      Map<String, Object> dict = new LinkedHashMap<>();
      dict.put("name", p.name());
      dict.put("kind", classifyPrimitive(p.javaType()));
      FieldMetadata fm = meta.fields().getOrDefault(p.name(), FieldMetadata.EMPTY);
      dict.put("description", fm.description());
      dict.put("tags", fm.tags());
      out.add(dict);
    }
    return out;
  }

  // ── v1 field schema extraction ────────────────────────────────────────────

  private static List<Map<String, Object>> fieldsFor(Class<?> cls, MethodMetadata meta) {
    if (cls == null) {
      return List.of();
    }
    List<Map<String, Object>> out = new ArrayList<>();
    if (cls.isRecord()) {
      for (RecordComponent rc : cls.getRecordComponents()) {
        out.add(fieldDict(rc.getName(), rc.getGenericType(), meta.fields()));
      }
      return out;
    }
    for (var f : cls.getDeclaredFields()) {
      if (java.lang.reflect.Modifier.isStatic(f.getModifiers()) || f.isSynthetic()) {
        continue;
      }
      out.add(fieldDict(f.getName(), f.getGenericType(), meta.fields()));
    }
    return out;
  }

  private static Map<String, Object> fieldDict(
      String name, Type t, Map<String, FieldMetadata> fieldMeta) {
    Map<String, Object> info = classify(t);
    Map<String, Object> dict = new LinkedHashMap<>();
    dict.put("name", name);
    dict.put("kind", info.get("kind"));
    dict.put("nullable", info.getOrDefault("nullable", false));
    // Records always provide all fields at construction time — every record field is "required"
    // from the manifest's perspective; scalar defaults would need metadata we don't introspect.
    dict.put("required", true);
    dict.put("default_value", null);
    dict.put("default_kind", "none");
    FieldMetadata fm = fieldMeta.getOrDefault(name, FieldMetadata.EMPTY);
    dict.put("description", fm.description());
    dict.put("tags", fm.tags());
    for (String key :
        List.of(
            "ref_name",
            "wire_tag",
            "enum_values",
            "item_kind",
            "item_ref",
            "item_wire_tag",
            "item_nullable",
            "key_kind",
            "value_kind",
            "value_ref",
            "value_nullable")) {
      if (info.containsKey(key)) {
        dict.put(key, info.get(key));
      }
    }
    dict.put("properties", Map.of());
    return dict;
  }

  private static Map<String, Object> classify(Type t) {
    Map<String, Object> out = new LinkedHashMap<>();
    boolean nullable = false;
    Type inner = t;
    if (t instanceof ParameterizedType pt
        && pt.getRawType() instanceof Class<?> rawCls
        && Optional.class.equals(rawCls)) {
      nullable = true;
      Type[] args = pt.getActualTypeArguments();
      if (args.length == 1) {
        inner = args[0];
      }
    }
    out.put("nullable", nullable);

    if (inner instanceof ParameterizedType pt && pt.getRawType() instanceof Class<?> rawCls) {
      if (Collection.class.isAssignableFrom(rawCls)) {
        out.put("kind", "list");
        Type[] args = pt.getActualTypeArguments();
        if (args.length == 1) {
          Map<String, Object> elem = classify(args[0]);
          out.put("item_kind", elem.getOrDefault("kind", "string"));
          out.put("item_nullable", elem.getOrDefault("nullable", false));
          if (elem.containsKey("ref_name")) {
            out.put("item_ref", elem.get("ref_name"));
          }
          if (elem.containsKey("wire_tag")) {
            out.put("item_wire_tag", elem.get("wire_tag"));
          }
        }
        return out;
      }
      if (Map.class.isAssignableFrom(rawCls)) {
        out.put("kind", "map");
        Type[] args = pt.getActualTypeArguments();
        if (args.length >= 2) {
          Map<String, Object> k = classify(args[0]);
          Map<String, Object> v = classify(args[1]);
          out.put("key_kind", k.getOrDefault("kind", "string"));
          out.put("value_kind", v.getOrDefault("kind", "string"));
          out.put("value_nullable", v.getOrDefault("nullable", false));
          if (v.containsKey("ref_name")) {
            out.put("value_ref", v.get("ref_name"));
          }
        }
        return out;
      }
    }

    Class<?> cls = (inner instanceof Class<?> c) ? c : null;
    if (cls == null
        && inner instanceof ParameterizedType pt
        && pt.getRawType() instanceof Class<?> raw) {
      cls = raw;
    }
    if (cls == null) {
      out.put("kind", "string");
      return out;
    }

    String primitive = classifyPrimitive(cls);
    if (!"ref".equals(primitive)) {
      out.put("kind", primitive);
      return out;
    }
    if (cls.isEnum()) {
      out.put("kind", "enum");
      out.put("ref_name", cls.getSimpleName());
      List<Object> values = new ArrayList<>();
      for (Object c : cls.getEnumConstants()) {
        values.add(((Enum<?>) c).name());
      }
      out.put("enum_values", values);
      return out;
    }
    out.put("kind", "ref");
    out.put("ref_name", cls.getSimpleName());
    out.put("wire_tag", WireIdentity.of(cls).packageName() + "/" + WireIdentity.of(cls).name());
    return out;
  }

  /** Primitive wire kind or "ref" if the class is not scalar. Matches Python's _classify_type. */
  private static String classifyPrimitive(Class<?> cls) {
    if (cls == null) {
      return "string";
    }
    if (cls == String.class || cls == CharSequence.class) {
      return "string";
    }
    if (cls == boolean.class || cls == Boolean.class) {
      return "bool";
    }
    if (cls == byte.class || cls == Byte.class) {
      return "int";
    }
    if (cls == short.class || cls == Short.class) {
      return "int";
    }
    if (cls == int.class || cls == Integer.class) {
      return "int";
    }
    if (cls == long.class || cls == Long.class) {
      return "int";
    }
    if (cls == float.class || cls == Float.class) {
      return "float";
    }
    if (cls == double.class || cls == Double.class) {
      return "float";
    }
    if (cls == byte[].class || cls == Byte[].class) {
      return "bytes";
    }
    return "ref";
  }

  // ── NFC comparator ────────────────────────────────────────────────────────

  static int nfcCompare(String a, String b) {
    int[] aa = Normalizer.normalize(a, Normalizer.Form.NFC).codePoints().toArray();
    int[] bb = Normalizer.normalize(b, Normalizer.Form.NFC).codePoints().toArray();
    int n = Math.min(aa.length, bb.length);
    for (int i = 0; i < n; i++) {
      int cmp = Integer.compare(aa[i], bb[i]);
      if (cmp != 0) {
        return cmp;
      }
    }
    return Integer.compare(aa.length, bb.length);
  }

  /** Convenience: compute just the {@code contract_id} for a dispatcher. */
  public static String computeContractId(ServiceDispatcher dispatcher) {
    return build(dispatcher).contractId();
  }
}
