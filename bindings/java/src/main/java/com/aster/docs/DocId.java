package com.aster.docs;

/** A document identifier. */
public record DocId(String hex) {

  /** Create from a hex string. */
  public static DocId of(String hex) {
    if (hex == null || hex.length() != 64) {
      throw new IllegalArgumentException("DocId must be 64 hex characters");
    }
    return new DocId(hex);
  }

  /** The hex string. */
  public String hex() {
    return hex;
  }
}
