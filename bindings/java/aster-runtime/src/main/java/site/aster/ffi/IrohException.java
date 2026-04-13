package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.ValueLayout;

/** Top-level exception for all FFI-level errors. */
public class IrohException extends RuntimeException {
  private final IrohStatus status;
  private final int errorCode;

  public IrohException(IrohStatus status, String message) {
    super(message);
    this.status = status;
    this.errorCode = -1;
  }

  public IrohException(IrohStatus status, int errorCode, String message) {
    super(message);
    this.status = status;
    this.errorCode = errorCode;
  }

  public IrohException(String message) {
    super(message);
    this.status = IrohStatus.INTERNAL;
    this.errorCode = -1;
  }

  public IrohStatus status() {
    return status;
  }

  public int errorCode() {
    return errorCode;
  }

  /**
   * Factory method that creates the most specific known {@link IrohException} subclass for the
   * given status code. Falls back to {@link IrohException} if the status has no specific subclass.
   *
   * @param status the status code
   * @param message the error message
   * @return a typed exception
   */
  public static IrohException forStatus(IrohStatus status, String message) {
    return switch (status) {
      case NOT_FOUND -> new site.aster.exception.IrohNotFoundException(message);
      case INVALID_ARGUMENT -> new site.aster.exception.IrohInvalidArgumentException(message);
      case TIMEOUT -> new site.aster.exception.IrohTimeoutException(message);
      case CANCELLED -> new site.aster.exception.IrohCancelledException(message);
      case CONNECTION_REFUSED -> new site.aster.exception.IrohConnectionRefusedException(message);
      case STREAM_RESET -> new site.aster.exception.IrohStreamResetException(message);
      case BUFFER_TOO_SMALL -> new site.aster.exception.IrohBufferTooSmallException(message);
      case ALREADY_CLOSED -> new site.aster.exception.IrohAlreadyClosedException(message);
      default -> new IrohException(status, message);
    };
  }

  /**
   * Returns the full message including the native error detail from {@code
   * iroh_last_error_message}. If no native error is available, returns just this exception's
   * message.
   */
  public String getMessageWithNativeDetail() {
    String nativeDetail = nativeErrorMessage();
    if (nativeDetail == null || nativeDetail.isEmpty()) {
      return getMessage();
    }
    return getMessage() + " — native: " + nativeDetail;
  }

  private static String nativeErrorMessage() {
    try {
      var lib = IrohLibrary.getInstance();
      var arena = Arena.ofAuto();
      var buf = arena.allocate(1024);
      long len = lib.lastErrorMessage(buf, 1024);
      if (len <= 0) {
        return "";
      }
      byte[] bytes = buf.asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
      return new String(bytes, java.nio.charset.StandardCharsets.UTF_8);
    } catch (Exception e) {
      return "";
    }
  }
}
