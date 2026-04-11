package com.aster.blobs;

/** Blob format for tickets. */
public enum BlobFormat {
  RAW(0),
  HASH_SEQ(1);

  public final int code;

  BlobFormat(int code) {
    this.code = code;
  }

  public static BlobFormat fromCode(int code) {
    for (var v : values()) {
      if (v.code == code) return v;
    }
    return RAW;
  }
}
