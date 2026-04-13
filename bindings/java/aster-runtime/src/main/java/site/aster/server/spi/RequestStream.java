package site.aster.server.spi;

/**
 * Source of request frames on a streaming dispatch. The dispatcher pulls already-framed payload
 * bytes; the runtime is responsible for reading from the reactor and demuxing into this queue.
 *
 * <p>All methods are blocking. Not required to be thread-safe.
 */
public interface RequestStream {

  /**
   * Return the next request frame payload, or {@code null} on end-of-stream.
   *
   * @throws Exception if the stream was cancelled or the peer errored
   */
  byte[] receive() throws Exception;
}
