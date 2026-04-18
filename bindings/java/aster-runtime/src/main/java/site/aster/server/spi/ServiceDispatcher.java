package site.aster.server.spi;

import java.util.List;
import java.util.Map;
import org.apache.fory.Fory;

/**
 * A generated dispatcher for one {@code @Service}-annotated class. Produced at build time by {@code
 * aster-codegen-apt} (Java) or {@code aster-codegen-ksp} (Kotlin) and discovered at runtime by
 * {@code AsterServer} via {@link java.util.ServiceLoader}.
 *
 * <p>One dispatcher instance per generated class, not per user instance. The dispatcher is
 * stateless; all state lives on the user's service instance.
 */
public interface ServiceDispatcher {

  /** Identity and lifetime metadata for the service. */
  ServiceDescriptor descriptor();

  /** Immutable map from method name to the method's dispatcher. */
  Map<String, MethodDispatcher> methods();

  /**
   * Register every request, response, and parameter type this service uses with the given Fory
   * instance. Called once when the dispatcher is bound to a codec. Implementations must swallow
   * "already registered" failures so user pre-registration wins.
   */
  void registerTypes(Fory fory);

  /**
   * Human-readable description of the service. Non-canonical: does not affect contract identity.
   *
   * <p>Default returns {@code ""} so existing generated dispatchers compiled against the older SPI
   * continue to satisfy the interface. The {@code aster-codegen-core} emitter overrides this with
   * the value from {@code @Service(description=...)} or the class Javadoc.
   */
  default String description() {
    return "";
  }

  /**
   * Service-level semantic tags. See {@code docs/_internal/rich_metadata/README.md} for the
   * conventional vocabulary. Default returns an empty list.
   */
  default List<String> tags() {
    return List.of();
  }

  /**
   * Per-method metadata lookup. Returns {@link MethodMetadata#EMPTY} when the dispatcher exposes no
   * metadata or {@code methodName} is not a declared method. The manifest publisher calls this once
   * per method when building the manifest JSON.
   */
  default MethodMetadata methodMetadata(String methodName) {
    return MethodMetadata.EMPTY;
  }
}
