package site.aster.blobs;

/**
 * An entry in a blob collection.
 *
 * @param name the entry name
 * @param hash the blob hash
 * @param size the blob size in bytes
 */
public record BlobEntry(String name, BlobId hash, long size) {}
