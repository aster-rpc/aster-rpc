package site.aster.docs;

/**
 * How a document sync round ended. A failed round has {@code succeeded} false and the document
 * layer's message in {@code error}; {@code error} is null for a completed round.
 */
public record DocSyncResult(DocSyncOrigin origin, boolean succeeded, String error) {}
