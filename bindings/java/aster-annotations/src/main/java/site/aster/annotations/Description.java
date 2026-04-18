package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Attach a description and optional tags to a field, record component, or inline RPC parameter.
 *
 * <p>Surfaces in the published {@code ContractManifest} JSON (per-field description/tags) and flows
 * into MCP tool schemas, generated client docstrings, and the shell's contract view. Non-canonical:
 * does not affect the contract identity hash.
 *
 * <p>Usage on a wire record component:
 *
 * <pre>{@code
 * public record HelloRequest(
 *     @Description("Name to greet.") String name,
 *     @Description(value = "API key", tags = {"secret"}) String apiKey) {}
 * }</pre>
 *
 * <p>Usage on a Mode 2 inline parameter:
 *
 * <pre>{@code
 * @Rpc
 * public Greeting greet(
 *     @Description("Name to greet.") String name,
 *     @Description(value = "BCP 47 locale.", tags = {"optional"}) String locale) { ... }
 * }</pre>
 *
 * <p>If absent, the APT/KSP processor falls back to the field/parameter Javadoc for the
 * description. Tags never have a Javadoc fallback — typos must be greppable.
 */
@Retention(RetentionPolicy.CLASS)
@Target({ElementType.PARAMETER, ElementType.FIELD, ElementType.RECORD_COMPONENT})
public @interface Description {
  String value();

  String[] tags() default {};
}
