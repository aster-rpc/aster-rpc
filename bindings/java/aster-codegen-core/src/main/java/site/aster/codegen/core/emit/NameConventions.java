package site.aster.codegen.core.emit;

import com.palantir.javapoet.ClassName;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ServiceModel;

/**
 * Central place for every name the emitters decide. Keeping them together makes it easy to align
 * APT and KSP output, and makes renames painless.
 */
public final class NameConventions {

  private NameConventions() {}

  /**
   * The generated dispatcher class name for a service: {@code {ServiceSimpleName}$AsterDispatcher}
   * in the same package as the user's service class.
   */
  public static ClassName dispatcherClassName(ServiceModel svc) {
    return ClassName.get(
        svc.implClass().packageName(), svc.implClass().simpleName() + "$AsterDispatcher");
  }

  /**
   * The synthesized {@code {ServiceSimpleName}_{MethodPascalCase}Request} record name for a method
   * classified as {@code INLINE}. Uses a leading-service prefix to avoid collisions between methods
   * named the same on two services in the same package.
   */
  public static ClassName inlineRequestClassName(ServiceModel svc, MethodModel method) {
    return ClassName.get(
        svc.implClass().packageName(),
        svc.implClass().simpleName() + "_" + pascalCase(method.name()) + "Request");
  }

  /**
   * Fory xlang type tag for a synthesized inline request. Uses the Java package with dots and the
   * {@code {MethodPascalCase}Request} class name.
   */
  public static String inlineRequestForyTag(ServiceModel svc, MethodModel method) {
    return svc.implClass().packageName() + "/" + pascalCase(method.name()) + "Request";
  }

  /** The per-method inner dispatcher class name: {@code {MethodPascalCase}$Dispatcher}. */
  public static String methodDispatcherSimpleName(MethodModel method) {
    return pascalCase(method.name()) + "$Dispatcher";
  }

  static String pascalCase(String name) {
    if (name == null || name.isEmpty()) {
      return name;
    }
    return Character.toUpperCase(name.charAt(0)) + name.substring(1);
  }
}
