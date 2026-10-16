package site.aster.codec;

import java.lang.reflect.Method;
import java.util.UUID;
import org.apache.fory.Fory;
import org.apache.fory.context.ReadContext;
import org.apache.fory.context.WriteContext;
import org.apache.fory.memory.MemoryBuffer;
import org.apache.fory.resolver.TypeResolver;
import org.apache.fory.serializer.Serializer;
import org.apache.fory.type.Types;

/**
 * The 16-byte identifier types of the contract ({@code uuid}, {@code ulid}) on the Fory xlang wire.
 * Fory 1.7 has no xlang UUID type, so — as in Rust ({@code [u8; 16]} with {@code #[aster(uuid)]})
 * and Python — the identifiers travel as Fory BINARY (type id 41): {@code varuint32} length 16,
 * then the bytes in RFC 9562 (big-endian) order. {@link #install} binds {@link UUID} and {@link
 * Ulid} to BINARY in a {@link Fory} instance, so a struct field of either type is described as
 * BINARY in the compatible-mode type meta and written as those 16 bytes. A decoder refuses any
 * other length.
 *
 * <p>Fory registers its built-in xlang types through a private {@code registerType(int, Class,
 * Serializer)}; there is no public way to give a class a built-in type id, so this calls it
 * reflectively (pinned to {@link ForyCodec#FORY_VERSION}).
 */
public final class ContractScalars {
  private ContractScalars() {}

  /** Bind {@link UUID} and {@link Ulid} to Fory BINARY in {@code fory} (idempotent). */
  public static void install(Fory fory) {
    TypeResolver resolver = fory.getTypeResolver();
    try {
      Method register =
          resolver
              .getClass()
              .getDeclaredMethod("registerType", int.class, Class.class, Serializer.class);
      register.setAccessible(true);
      register.invoke(resolver, Types.BINARY, UUID.class, new UuidSerializer(fory));
      register.invoke(resolver, Types.BINARY, Ulid.class, new UlidSerializer(fory));
    } catch (ReflectiveOperationException e) {
      throw new IllegalStateException(
          "cannot bind uuid / ulid to Fory BINARY (Fory " + ForyCodec.FORY_VERSION + ")", e);
    }
  }

  static byte[] uuidBytes(UUID u) {
    byte[] b = new byte[16];
    long hi = u.getMostSignificantBits();
    long lo = u.getLeastSignificantBits();
    for (int i = 0; i < 8; i++) {
      b[i] = (byte) (hi >>> (56 - 8 * i));
      b[8 + i] = (byte) (lo >>> (56 - 8 * i));
    }
    return b;
  }

  static UUID uuidOf(byte[] b) {
    long hi = 0;
    long lo = 0;
    for (int i = 0; i < 8; i++) {
      hi = (hi << 8) | (b[i] & 0xff);
      lo = (lo << 8) | (b[8 + i] & 0xff);
    }
    return new UUID(hi, lo);
  }

  private static byte[] read16(ReadContext ctx, String what) {
    MemoryBuffer buffer = ctx.getBuffer();
    int size = buffer.readVarUInt32Small7();
    if (size != 16) {
      throw new IllegalArgumentException(what + " must be 16 bytes on the wire, got " + size);
    }
    byte[] out = new byte[16];
    buffer.readBytes(out);
    return out;
  }

  static final class UuidSerializer extends Serializer<UUID> {
    UuidSerializer(Fory fory) {
      super(fory.getConfig(), UUID.class, true);
    }

    @Override
    public void write(WriteContext ctx, UUID value) {
      ctx.getBuffer().writeBytesWithSize(uuidBytes(value));
    }

    @Override
    public UUID read(ReadContext ctx) {
      return uuidOf(read16(ctx, "a uuid"));
    }
  }

  static final class UlidSerializer extends Serializer<Ulid> {
    UlidSerializer(Fory fory) {
      super(fory.getConfig(), Ulid.class, true);
    }

    @Override
    public void write(WriteContext ctx, Ulid value) {
      ctx.getBuffer().writeBytesWithSize(value.toBytes());
    }

    @Override
    public Ulid read(ReadContext ctx) {
      return Ulid.fromBytes(read16(ctx, "a ulid"));
    }
  }
}
