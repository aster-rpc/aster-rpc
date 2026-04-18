package site.aster.contract;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Base64;
import java.util.Collection;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import site.aster.blobs.BlobId;
import site.aster.blobs.IrohBlobs;
import site.aster.docs.AuthorId;
import site.aster.docs.Doc;
import site.aster.docs.Docs;
import site.aster.node.IrohNode;
import site.aster.registry.ArtifactRef;
import site.aster.registry.RegistryAsync;
import site.aster.registry.RegistryKeys;
import site.aster.server.spi.ServiceDispatcher;

/**
 * Publishes each registered service's contract collection into a newly-created registry doc, so
 * consumer-side tooling ({@code aster contract gen-client}, the registry shell, etc.) can fetch
 * manifests by {@code contract_id}. Mirrors Python's {@code AsterServer._publish_contracts} in
 * {@code bindings/python/aster/runtime.py} (lines 624-750) — same layout, same keys:
 *
 * <ul>
 *   <li>One shared registry doc + author for the whole server.
 *   <li>Per service: upload a HashSeq collection containing {@code contract.bin}, {@code
 *       types/<hash>.bin}, {@code manifest.json}; write an {@link ArtifactRef} to {@code
 *       contracts/<contract_id>}; write the manifest JSON directly to {@code
 *       manifests/<contract_id>} so shell / fast-access readers can skip the blob fetch; bump the
 *       {@code services/<name>/versions/v<n>} pointer via {@link RegistryAsync#publishAsync}.
 *   <li>Doc shared read-only so remote consumers can join + sync the namespace.
 * </ul>
 *
 * <p>Dev-mode open-gate: no signing, no ACL. Auth-mode publishing is tracked under tasks #14/#15.
 */
public final class ContractPublisher {

  /** The registry namespace + per-service contract ids produced by a successful publish. */
  public record Published(String registryNamespace, Map<String, String> contractIds) {}

  private ContractPublisher() {}

  /**
   * Create a fresh registry doc, upload every dispatcher's contract collection, and share the doc
   * read-only. The returned future completes with the registry namespace + contract ids after the
   * share finishes.
   */
  public static CompletableFuture<Published> publishAll(
      IrohNode node, Collection<ServiceDispatcher> dispatchers) {
    Docs docs = node.docs();
    IrohBlobs blobs = node.blobs();

    return docs.createAsync()
        .thenCompose(
            registryDoc ->
                docs.createAuthorAsync()
                    .thenCompose(author -> publishChain(registryDoc, author, blobs, dispatchers))
                    .thenCompose(
                        result ->
                            registryDoc
                                .shareAsync(0)
                                .thenApply(
                                    ticketIgnored ->
                                        new Published(registryDoc.docId(), result.contractIds()))));
  }

  private static CompletableFuture<PublishedState> publishChain(
      Doc registryDoc,
      AuthorId author,
      IrohBlobs blobs,
      Collection<ServiceDispatcher> dispatchers) {
    Map<String, String> contractIds = new LinkedHashMap<>();
    CompletableFuture<Void> chain = CompletableFuture.completedFuture(null);
    for (ServiceDispatcher d : dispatchers) {
      chain = chain.thenCompose(v -> publishOne(registryDoc, author, blobs, d, contractIds));
    }
    return chain.thenApply(v -> new PublishedState(contractIds));
  }

  private record PublishedState(Map<String, String> contractIds) {}

  private static CompletableFuture<Void> publishOne(
      Doc registryDoc,
      AuthorId author,
      IrohBlobs blobs,
      ServiceDispatcher dispatcher,
      Map<String, String> contractIds) {
    ContractManifestBuilder.PublicationArtifacts artifacts =
        ContractManifestBuilder.buildForPublication(dispatcher);

    String entriesJson = buildCollectionJson(artifacts);
    contractIds.put(artifacts.serviceName(), artifacts.contractId());

    return blobs
        .addCollectionAsync(entriesJson)
        .thenCompose(
            collectionHash ->
                writeArtifactRef(registryDoc, author, artifacts, collectionHash)
                    .thenCompose(v -> writeManifestBytes(registryDoc, author, artifacts)));
  }

  /**
   * Build the JSON payload accepted by {@code iroh_blobs_add_collection}: a JSON array of {@code
   * [name, base64-data]} pairs, in deterministic order (contract.bin, then sorted types/*.bin, then
   * manifest.json — same order as Python's {@code build_collection}).
   */
  static String buildCollectionJson(ContractManifestBuilder.PublicationArtifacts artifacts) {
    Base64.Encoder enc = Base64.getEncoder();
    List<List<String>> entries = new ArrayList<>();
    entries.add(List.of("contract.bin", enc.encodeToString(artifacts.contractCanonicalBytes())));
    for (Map.Entry<String, byte[]> e : artifacts.typeEntries()) {
      entries.add(List.of(e.getKey(), enc.encodeToString(e.getValue())));
    }
    entries.add(List.of("manifest.json", enc.encodeToString(artifacts.manifestJsonBytes())));
    try {
      return new ObjectMapper().writeValueAsString(entries);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException("Failed to encode collection entries JSON", e);
    }
  }

  private static CompletableFuture<Void> writeArtifactRef(
      Doc registryDoc,
      AuthorId author,
      ContractManifestBuilder.PublicationArtifacts artifacts,
      BlobId collectionHash) {
    ArtifactRef ref = new ArtifactRef();
    ref.contractId = artifacts.contractId();
    ref.collectionHash = collectionHash.hex();
    ref.publishedBy = author.hex();
    ref.publishedAtEpochMs = System.currentTimeMillis();
    ref.collectionFormat = "index";

    // Use the unified aster_registry_publish FFI — one Rust call writes the ArtifactRef +
    // version pointer + (optional) channel alias + gossip in a single op. Matches the
    // `Published contract` step in Python's _publish_contracts, minus the manifests/<id>
    // fast-access key, which we write separately below.
    return RegistryAsync.publishAsync(
        registryDoc,
        author.hex(),
        null,
        ref,
        artifacts.serviceName(),
        artifacts.serviceVersion(),
        null,
        null);
  }

  private static CompletableFuture<Void> writeManifestBytes(
      Doc registryDoc, AuthorId author, ContractManifestBuilder.PublicationArtifacts artifacts) {
    String manifestKey = "manifests/" + artifacts.contractId();
    return registryDoc
        .setBytesAsync(author, manifestKey, artifacts.manifestJsonBytes())
        .thenApply(v -> null);
  }

  /**
   * Helper: the canonical registry-sync prefixes ({@link RegistryKeys#REGISTRY_PREFIXES}) expressed
   * as UTF-8 strings for consumers that need to add a matching download policy before joining the
   * registry namespace. Returned as an immutable list for safe reuse.
   */
  public static List<String> registryPrefixStrings() {
    List<String> out = new ArrayList<>();
    for (byte[] prefix : RegistryKeys.REGISTRY_PREFIXES) {
      out.add(new String(prefix, StandardCharsets.UTF_8));
    }
    return Collections.unmodifiableList(out);
  }
}
