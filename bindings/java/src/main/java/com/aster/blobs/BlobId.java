package com.aster.blobs;

import java.util.HexFormat;

/** A 32-byte blob identifier, displayed as a 64-character hex string. */
public record BlobId(String hex) {

  public BlobId {
    if (hex != null && hex.length() != 64) {
      throw new IllegalArgumentException("BlobId must be 64 hex characters, got: " + hex.length());
    }
  }

  /**
   * Parse a hex string into a BlobId.
   *
   * @param hex 64-character hex string
   * @return the BlobId
   */
  public static BlobId of(String hex) {
    return new BlobId(hex);
  }

  /**
   * Create a BlobId from raw bytes.
   *
   * @param bytes 32-byte array
   * @return the BlobId
   */
  public static BlobId of(byte[] bytes) {
    if (bytes.length != 32) {
      throw new IllegalArgumentException("BlobId must be 32 bytes, got: " + bytes.length);
    }
    return new BlobId(HexFormat.of().formatHex(bytes));
  }

  /** Returns the underlying hex string. */
  public String hex() {
    return hex;
  }
}
