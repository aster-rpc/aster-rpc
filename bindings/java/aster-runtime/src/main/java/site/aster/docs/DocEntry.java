package site.aster.docs;

import site.aster.blobs.BlobId;

/** An entry in a document. */
public record DocEntry(String key, AuthorId author, BlobId contentHash, byte[] value) {}
