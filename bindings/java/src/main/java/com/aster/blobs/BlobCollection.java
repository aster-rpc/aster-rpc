package com.aster.blobs;

import java.util.List;

/**
 * A collection of named blobs.
 *
 * @param entries the list of blob entries in this collection
 */
public record BlobCollection(List<BlobEntry> entries) {}
