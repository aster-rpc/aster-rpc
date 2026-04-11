package com.aster.docs;

import com.aster.blobs.BlobId;

/** An entry in a document. */
public record DocEntry(String key, AuthorId author, BlobId contentHash, byte[] value) {}
