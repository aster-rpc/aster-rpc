package site.aster.blobs;

/**
 * Local information about a blob.
 *
 * @param hash the blob hash
 * @param size the total size in bytes
 * @param status the blob status
 */
public record BlobInfo(BlobId hash, long size, BlobStatus status) {}
