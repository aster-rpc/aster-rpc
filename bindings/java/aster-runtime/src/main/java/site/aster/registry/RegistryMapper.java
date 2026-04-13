package site.aster.registry;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.SerializationFeature;

/** Shared Jackson ObjectMapper for registry wire types. */
final class RegistryMapper {

  static final ObjectMapper MAPPER =
      new ObjectMapper()
          .setSerializationInclusion(JsonInclude.Include.ALWAYS)
          .disable(SerializationFeature.WRITE_DATES_AS_TIMESTAMPS)
          .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES);

  private RegistryMapper() {}
}
