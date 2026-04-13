package site.aster.server.spi;

/**
 * Describes a single inline parameter of a Mode 2 handler.
 *
 * @param name the parameter name as declared in the user's source
 * @param typeTag the Fory xlang type tag (e.g. {@code "string"}, {@code "int32"}, {@code
 *     "example/AgentConfig"})
 * @param javaType the resolved Java type reference (for code-gen emission and runtime Fory
 *     registration)
 */
public record ParamDescriptor(String name, String typeTag, Class<?> javaType) {}
