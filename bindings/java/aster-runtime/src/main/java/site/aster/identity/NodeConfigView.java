package site.aster.identity;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.List;
import java.util.Map;

/**
 * {@code aster.ops.NodeConfig} get (Rust {@code NodeConfigView}): the record and every overridable
 * setting's effective value.
 */
public record NodeConfigView(
    String nodeId,
    long generation,
    long appliedGeneration,
    String change,
    String changedBy,
    long changedAtMs,
    String name,
    List<String> tags,
    Map<String, NodeAttribute> attributes,
    Map<String, JsonNode> sections,
    List<String> ignoredSections,
    List<EffectiveSetting> settings,
    List<String> overridableSections,
    List<String> lockedSections) {

  public NodeConfigView {
    tags = tags == null ? List.of() : List.copyOf(tags);
    attributes = attributes == null ? Map.of() : Map.copyOf(attributes);
    sections = sections == null ? Map.of() : Map.copyOf(sections);
    ignoredSections = ignoredSections == null ? List.of() : List.copyOf(ignoredSections);
    settings = settings == null ? List.of() : List.copyOf(settings);
    overridableSections =
        overridableSections == null ? List.of() : List.copyOf(overridableSections);
    lockedSections = lockedSections == null ? List.of() : List.copyOf(lockedSections);
  }

  /**
   * One overridable setting's value in force and where it came from.
   *
   * @param key {@code section.field}, e.g. {@code identity.accept_on_behalf_of}
   * @param source {@code "default"}, {@code "file"}, {@code "env"}, {@code "code"} or {@code
   *     "override"}
   */
  public record EffectiveSetting(String key, JsonNode value, String source) {}

  /** One setting by {@code section.field} key, or {@code null}. */
  public EffectiveSetting setting(String key) {
    for (EffectiveSetting s : settings) {
      if (s.key().equals(key)) {
        return s;
      }
    }
    return null;
  }
}
