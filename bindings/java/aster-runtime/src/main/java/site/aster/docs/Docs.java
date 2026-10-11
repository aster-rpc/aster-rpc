package site.aster.docs;

import java.nio.charset.StandardCharsets;
import java.util.concurrent.CompletableFuture;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;
import site.aster.node.IrohNode;

/** Document operations for an Iroh node; obtain via {@link IrohNode#docs}. */
public class Docs {
  private final IrohRuntime runtime;
  private final long nodeHandle;

  public Docs(IrohRuntime runtime, long nodeHandle) {
    this.runtime = runtime;
    this.nodeHandle = nodeHandle;
  }

  /** Create a new namespace. */
  public CompletableFuture<Doc> createAsync() {
    return DataPlane.docsCreate(runtime, nodeHandle)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_CREATED)
                throw new IrohException("unexpected event " + r.event().kind());
              return new Doc(runtime, r.event().handle(), nodeHandle);
            });
  }

  /** Create a signing author. */
  public CompletableFuture<AuthorId> createAuthorAsync() {
    return DataPlane.docsCreateAuthor(runtime, nodeHandle)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.AUTHOR_CREATED)
                throw new IrohException("unexpected event " + r.event().kind());
              return AuthorId.of(new String(r.data(), StandardCharsets.UTF_8));
            });
  }

  /** Join a namespace from a ticket. */
  public CompletableFuture<Doc> joinAsync(String ticket) {
    return DataPlane.docsJoin(runtime, nodeHandle, ticket, false)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_JOINED)
                throw new IrohException("unexpected event " + r.event().kind());
              return new Doc(runtime, r.event().handle(), nodeHandle);
            });
  }

  /** Join a namespace and own both its document and event receiver. Close the returned owner. */
  public CompletableFuture<DocSubscription> joinAndSubscribeAsync(String ticket) {
    return DocSubscription.join(runtime, nodeHandle, ticket);
  }

  // ── ABI 3.3 data plane ──────────────────────────────────────────────────

  private Doc opened(DataPlane.Result r) {
    if (r.event().kind() != IrohEventKind.DOC_OPENED) {
      throw new IrohException("unexpected event " + r.event().kind());
    }
    return new Doc(runtime, r.event().handle(), nodeHandle);
  }

  /**
   * Report the authority this node holds over the namespace {@code namespaceId} names, read back
   * from the store. Fails if the namespace is not in the local store.
   */
  public CompletableFuture<AuthorityReport> authorityReportAsync(String namespaceId) {
    return DataPlane.docsAuthorityReport(runtime, nodeHandle, namespaceId)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_AUTHORITY_REPORT) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              return AuthorityReport.parse(r.event().flags(), r.data());
            });
  }

  /** The authority fingerprint of {@code (namespaceId, access)}, as an authority report carries. */
  public static byte[] authorityFingerprint(String namespaceId, LocalAccess access) {
    return DataPlane.docAuthorityFingerprint(namespaceId, access.code);
  }

  /**
   * Retire this node's <b>local</b> write authority over a namespace, keeping every entry. Release
   * every reference first ({@link Doc#shutdownForRoleSwapAsync}); otherwise the outcome is busy.
   */
  public CompletableFuture<RetireOutcome> retireLocalWriteAuthorityAsync(String namespaceId) {
    return DataPlane.docsRetireLocalWriteAuthority(runtime, nodeHandle, namespaceId)
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.DOC_RETIRE_OUTCOME) {
                throw new IrohException("unexpected event " + r.event().kind());
              }
              var b = java.nio.ByteBuffer.wrap(r.data()).order(java.nio.ByteOrder.LITTLE_ENDIAN);
              int access = b.getInt(0);
              long handles = b.getLong(8);
              boolean retired = r.event().flags() == 0;
              return new RetireOutcome(
                  retired, retired ? LocalAccess.fromCode(access) : null, handles);
            });
  }

  /** Reopen a namespace already in the local replica; empty if it is not present. */
  public CompletableFuture<java.util.Optional<Doc>> openAsync(String namespaceId) {
    return DataPlane.docsOpen(runtime, nodeHandle, namespaceId)
        .thenApply(
            r -> {
              if (r.event().kind() == IrohEventKind.DOC_OPENED
                  && r.event().status() == IrohStatus.NOT_FOUND.code) {
                return java.util.Optional.empty();
              }
              return java.util.Optional.of(opened(r));
            });
  }

  /**
   * Open-or-import a writable namespace from its 32-byte secret. Idempotent; upgrades a prior
   * read-only import.
   */
  public CompletableFuture<Doc> importWriteNamespaceAsync(byte[] secret) {
    return DataPlane.docsImportWriteNamespace(runtime, nodeHandle, secret).thenApply(this::opened);
  }

  /**
   * Open-or-import a read-only namespace from its id. Idempotent; never narrows an existing write
   * capability.
   */
  public CompletableFuture<Doc> importReadNamespaceAsync(String namespaceId) {
    return DataPlane.docsImportReadNamespace(runtime, nodeHandle, namespaceId)
        .thenApply(this::opened);
  }
}
