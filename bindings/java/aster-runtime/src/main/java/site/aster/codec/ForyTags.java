package site.aster.codec;

import org.apache.fory.BaseFory;

/**
 * Helpers for registering types with Aster-style slash-separated wire tags like {@code
 * "_aster/RpcStatus"} or {@code "mission/StatusRequest"}.
 *
 * <p>Why this exists: Apache Fory's 2-arg {@link BaseFory#register(Class, String)} splits the tag
 * on the last {@code .} (ASCII 46) to derive the (namespace, typename) pair it encodes on the wire
 * — NOT on {@code /}. Aster's convention, matched by Python's {@code @wire_type}, puts the
 * namespace on the left of a {@code /}. Passing {@code "_aster/RpcStatus"} to Fory's 2-arg form
 * therefore registers with namespace={@code ""} and typename={@code "_aster/RpcStatus"}, which
 * produces wire bytes the Python side can't decode (pyfory sees ns={@code ""} and looks up a
 * typename nobody registered → falls back to {@code importlib.import_module("")} and raises {@code
 * ValueError: Empty module name}).
 *
 * <p>{@link #register} splits the tag on the last {@code /}, invokes Fory's explicit 3-arg {@code
 * register(cls, namespace, typename)} form, and the resulting wire matches Python byte-for-byte.
 */
public final class ForyTags {

  private ForyTags() {}

  /**
   * Register {@code cls} with Fory using the Aster slash-separated tag convention. Splits on the
   * last {@code /} into {@code (namespace, typename)}; a tag with no slash maps to {@code (empty,
   * tag)}; a null/empty tag registers without a tag at all.
   */
  public static void register(BaseFory fory, Class<?> cls, String tag) {
    if (tag == null || tag.isEmpty()) {
      fory.register(cls);
      return;
    }
    int slash = tag.lastIndexOf('/');
    if (slash >= 0) {
      fory.register(cls, tag.substring(0, slash), tag.substring(slash + 1));
    } else {
      fory.register(cls, tag);
    }
  }
}
