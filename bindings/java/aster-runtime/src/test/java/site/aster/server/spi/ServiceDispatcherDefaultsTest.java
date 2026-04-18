package site.aster.server.spi;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;
import org.apache.fory.Fory;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;

/**
 * Pins backwards compatibility for hand-written {@link ServiceDispatcher} implementations that
 * haven't been regenerated since rich metadata landed. A hand-written dispatcher that only
 * implements the required three methods (descriptor / methods / registerTypes) must continue to
 * compile AND return sensible empty defaults for description / tags / methodMetadata.
 */
final class ServiceDispatcherDefaultsTest {

  @Test
  void minimalDispatcherExposesEmptyMetadataViaDefaults() {
    ServiceDispatcher d =
        new ServiceDispatcher() {
          @Override
          public ServiceDescriptor descriptor() {
            return new ServiceDescriptor("hand", 1, Scope.SHARED, Object.class);
          }

          @Override
          public Map<String, MethodDispatcher> methods() {
            return Map.of();
          }

          @Override
          public void registerTypes(Fory fory) {}
        };

    assertEquals("", d.description());
    assertEquals(List.of(), d.tags());
    assertSame(MethodMetadata.EMPTY, d.methodMetadata("anything"));
  }

  @Test
  void dispatcherCanOverrideMetadataMethods() {
    ServiceDispatcher d =
        new ServiceDispatcher() {
          @Override
          public ServiceDescriptor descriptor() {
            return new ServiceDescriptor("hand", 1, Scope.SHARED, Object.class);
          }

          @Override
          public Map<String, MethodDispatcher> methods() {
            return Map.of();
          }

          @Override
          public void registerTypes(Fory fory) {}

          @Override
          public String description() {
            return "hand-rolled";
          }

          @Override
          public List<String> tags() {
            return List.of("custom");
          }

          @Override
          public MethodMetadata methodMetadata(String methodName) {
            if ("foo".equals(methodName)) {
              return new MethodMetadata(
                  "bar",
                  List.of("readonly"),
                  false,
                  Map.of("arg", new FieldMetadata("an arg", List.of("pii"))));
            }
            return MethodMetadata.EMPTY;
          }
        };

    assertEquals("hand-rolled", d.description());
    assertEquals(List.of("custom"), d.tags());
    MethodMetadata meta = d.methodMetadata("foo");
    assertEquals("bar", meta.description());
    assertTrue(meta.tags().contains("readonly"));
    assertEquals("an arg", meta.fields().get("arg").description());
    assertTrue(meta.fields().get("arg").tags().contains("pii"));

    // Unknown method → EMPTY sentinel
    assertSame(MethodMetadata.EMPTY, d.methodMetadata("doesNotExist"));
  }
}
