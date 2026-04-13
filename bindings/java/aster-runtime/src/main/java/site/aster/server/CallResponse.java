package site.aster.server;

/**
 * Response to an Aster RPC call.
 *
 * @param responseFrame the response payload bytes (written as a single frame)
 * @param trailerFrame optional trailer bytes (may be empty)
 */
public record CallResponse(byte[] responseFrame, byte[] trailerFrame) {

  /** Create a response with no trailer. */
  public static CallResponse of(byte[] response) {
    return new CallResponse(response, new byte[0]);
  }

  /** Create a response with a trailer. */
  public static CallResponse of(byte[] response, byte[] trailer) {
    return new CallResponse(response, trailer);
  }
}
