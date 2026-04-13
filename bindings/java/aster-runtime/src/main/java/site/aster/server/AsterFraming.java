package site.aster.server;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * Aster wire framing utilities.
 *
 * <p>Wire format: {@code [4-byte LE frame_body_len][1-byte flags][payload]}
 *
 * <p>Where {@code frame_body_len = 1 (flags byte) + payload.length}.
 */
public final class AsterFraming {

  public static final byte FLAG_COMPRESSED = 0x01;
  public static final byte FLAG_TRAILER = 0x02;
  public static final byte FLAG_HEADER = 0x04;
  public static final byte FLAG_ROW_SCHEMA = 0x08;
  public static final byte FLAG_CALL = 0x10;
  public static final byte FLAG_CANCEL = 0x20;

  /**
   * Set on the LAST request frame of a client-streaming or bidi-streaming call. Tells the reactor
   * to close the per-call request channel and stop reading more request frames for this call.
   */
  public static final byte FLAG_END_STREAM = 0x40;

  private AsterFraming() {}

  /**
   * Encode a frame: returns {@code [4-byte LE length][1-byte flags][payload]}.
   *
   * @param payload the frame payload bytes
   * @param flags the frame flags byte
   * @return the encoded frame bytes
   */
  public static byte[] encodeFrame(byte[] payload, byte flags) {
    int frameBodyLen = 1 + payload.length;
    byte[] frame = new byte[4 + frameBodyLen];
    ByteBuffer.wrap(frame).order(ByteOrder.LITTLE_ENDIAN).putInt(frameBodyLen);
    frame[4] = flags;
    System.arraycopy(payload, 0, frame, 5, payload.length);
    return frame;
  }
}
