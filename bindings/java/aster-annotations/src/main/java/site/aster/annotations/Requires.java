package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Declares the capability requirement for an RPC method or a whole service. When present, the
 * server-side {@code CapabilityInterceptor} rejects calls whose {@code CallContext.attributes} do
 * not carry the required role(s) under the canonical {@code aster.role} key.
 *
 * <p>Mirrors Python's {@code @rpc(requires=...)} / {@code @service(requires=...)}. Both
 * service-level and method-level requirements are checked (conjunction). A method-level
 * {@code @Requires} does not override a service-level one; both must be satisfied.
 *
 * <p>Example:
 *
 * <pre>{@code
 * @Service(name = "MissionControl")
 * class Producer {
 *   @Rpc
 *   @Requires(roles = "ops.status")
 *   public StatusResponse getStatus(StatusRequest req) { ... }
 *
 *   @ServerStream
 *   @Requires(kind = RequiresKind.ANY_OF, roles = {"ops.logs", "ops.admin"})
 *   public void tailLogs(TailRequest req, ResponseStream<LogEntry> out) { ... }
 * }
 * }</pre>
 */
@Retention(RetentionPolicy.CLASS)
@Target({ElementType.METHOD, ElementType.TYPE})
public @interface Requires {

  /**
   * Required role strings. For {@link RequiresKind#ROLE}, must be a singleton array. For {@link
   * RequiresKind#ANY_OF} / {@link RequiresKind#ALL_OF}, any non-empty list.
   */
  String[] roles();

  /**
   * How to evaluate {@link #roles()} against the caller's attributes. Default: {@link
   * RequiresKind#ROLE}.
   */
  RequiresKind kind() default RequiresKind.ROLE;
}
