package com.aster.blobs;

/**
 * A ticket string for sharing or downloading blobs.
 *
 * <p>Format: {@code blob1...} prefix followed by base64-encoded data.
 */
public record BlobTicket(String ticket) {

  public BlobTicket {
    if (ticket == null || ticket.isEmpty()) {
      throw new IllegalArgumentException("ticket cannot be null or empty");
    }
  }

  /**
   * Parse a ticket string.
   *
   * @param ticket the ticket string
   * @return the BlobTicket
   */
  public static BlobTicket of(String ticket) {
    return new BlobTicket(ticket);
  }
}
