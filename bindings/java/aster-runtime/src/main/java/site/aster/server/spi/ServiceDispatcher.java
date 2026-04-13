package site.aster.server.spi;

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
}
