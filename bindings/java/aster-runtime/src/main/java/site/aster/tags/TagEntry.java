package site.aster.tags;

import site.aster.blobs.BlobId;

/** Information about a named tag in the blob store. */
public record TagEntry(String name, BlobId hash, TagFormat format) {}
