package com.aster.registry;

import com.aster.docs.Doc;
import com.aster.ffi.IrohEventKind;
import com.aster.ffi.IrohException;
import com.aster.ffi.IrohLibrary;
import com.aster.ffi.IrohStatus;
import com.aster.gossip.IrohGossip;
import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.core.type.TypeReference;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.CompletableFuture;

/**
 * Async doc-backed registry operations (§11.8). These complement the synchronous filter/rank
 * methods in {@link Registry} by going through the bridge tokio runtime: each call submits an FFI
 * op and awaits the matching event (kinds 80-84) on the runtime event pump.
 *
 * <p>Round-robin rotation and stale-seq filtering are persistent on the Rust bridge, so callers do
 * not need to maintain their own state.
 */
public final class RegistryAsync {

  private static final TypeReference<List<String>> STRING_LIST = new TypeReference<>() {};

  private RegistryAsync() {}

  /**
   * Run the full resolve pipeline (pointer lookup, list_leases, monotonic seq filter, mandatory
   * filters, rank) for the given options against the given registry doc. The future completes with
   * the winning lease, or {@code null} if no candidate survived.
   */
  public static CompletableFuture<EndpointLease> resolveAsync(
      Doc doc, Registry.ResolveOptions opts) {
    var lib = IrohLibrary.getInstance();
    byte[] optsJson;
    try {
      optsJson = RegistryMapper.MAPPER.writeValueAsBytes(opts);
    } catch (JsonProcessingException e) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException("failed to encode ResolveOptions", e));
    }
    Arena arena = Arena.ofConfined();
    long opId;
    try {
      MemorySegment optsSeg = arena.allocate(optsJson.length);
      optsSeg.copyFrom(MemorySegment.ofArray(optsJson));
      MemorySegment opSeg = arena.allocate(ValueLayout.JAVA_LONG);
      int status =
          lib.asterRegistryResolve(
              doc.runtime().nativeHandle(), doc.docHandle(), optsSeg, optsJson.length, 0L, opSeg);
      if (status != 0) {
        arena.close();
        return CompletableFuture.failedFuture(
            new IrohException(IrohStatus.fromCode(status), "aster_registry_resolve: " + status));
      }
      opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      arena.close();
      return CompletableFuture.failedFuture(
          new IrohException("aster_registry_resolve threw: " + t.getMessage()));
    }
    return doc.runtime()
        .registry()
        .register(opId)
        .whenComplete((unused, err) -> arena.close())
        .thenApply(
            event -> {
              if (event.kind() != IrohEventKind.REGISTRY_RESOLVED) {
                throw new IrohException("resolve: unexpected event " + event.kind());
              }
              if (event.status() == IrohStatus.NOT_FOUND.code) {
                return null;
              }
              byte[] payload = event.data().asByteBuffer().array();
              try {
                return RegistryMapper.MAPPER.readValue(payload, EndpointLease.class);
              } catch (java.io.IOException e) {
                throw new IrohException("resolve: failed to decode lease: " + e.getMessage());
              }
            });
  }

  /**
   * Publish a lease and/or an artifact in a single op. Either may be null to skip; at least one
   * must be supplied. When publishing an artifact, {@code service} and {@code version} are
   * required. {@code topic} is optional gossip topic to broadcast on.
   */
  public static CompletableFuture<Void> publishAsync(
      Doc doc,
      String authorId,
      EndpointLease lease,
      ArtifactRef artifact,
      String service,
      int version,
      String channel,
      IrohGossip.GossipTopic topic) {
    if (lease == null && artifact == null) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException("publishAsync requires at least one of lease or artifact"));
    }
    var lib = IrohLibrary.getInstance();
    byte[] author = authorId.getBytes(StandardCharsets.UTF_8);
    byte[] leaseBytes;
    byte[] artifactBytes;
    try {
      leaseBytes = lease == null ? new byte[0] : RegistryMapper.MAPPER.writeValueAsBytes(lease);
      artifactBytes =
          artifact == null ? new byte[0] : RegistryMapper.MAPPER.writeValueAsBytes(artifact);
    } catch (JsonProcessingException e) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException("failed to encode publish payload", e));
    }
    byte[] svc = service == null ? new byte[0] : service.getBytes(StandardCharsets.UTF_8);
    byte[] chn = channel == null ? new byte[0] : channel.getBytes(StandardCharsets.UTF_8);
    long topicHandle = topic == null ? 0L : topic.handle();

    Arena arena = Arena.ofConfined();
    long opId;
    try {
      MemorySegment authorSeg = bytesToSegment(arena, author);
      MemorySegment leaseSeg = bytesToSegment(arena, leaseBytes);
      MemorySegment artifactSeg = bytesToSegment(arena, artifactBytes);
      MemorySegment svcSeg = bytesToSegment(arena, svc);
      MemorySegment chnSeg = bytesToSegment(arena, chn);
      MemorySegment opSeg = arena.allocate(ValueLayout.JAVA_LONG);
      int status =
          lib.asterRegistryPublish(
              doc.runtime().nativeHandle(),
              doc.docHandle(),
              authorSeg,
              author.length,
              leaseSeg,
              leaseBytes.length,
              artifactSeg,
              artifactBytes.length,
              svcSeg,
              svc.length,
              version,
              chnSeg,
              chn.length,
              topicHandle,
              0L,
              opSeg);
      if (status != 0) {
        arena.close();
        return CompletableFuture.failedFuture(
            new IrohException(IrohStatus.fromCode(status), "aster_registry_publish: " + status));
      }
      opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      arena.close();
      return CompletableFuture.failedFuture(
          new IrohException("aster_registry_publish threw: " + t.getMessage()));
    }
    return doc.runtime()
        .registry()
        .register(opId)
        .whenComplete((u, e) -> arena.close())
        .thenApply(
            event -> {
              if (event.kind() != IrohEventKind.REGISTRY_PUBLISHED) {
                throw new IrohException("publish: unexpected event " + event.kind());
              }
              return null;
            });
  }

  /**
   * Renew an existing lease in place. Pass {@link Float#NaN} for {@code load} to leave it unset.
   */
  public static CompletableFuture<Void> renewLeaseAsync(
      Doc doc,
      String authorId,
      String service,
      String contractId,
      String endpointId,
      String health,
      float load,
      int leaseDurationS,
      IrohGossip.GossipTopic topic) {
    var lib = IrohLibrary.getInstance();
    byte[] author = authorId.getBytes(StandardCharsets.UTF_8);
    byte[] svc = service.getBytes(StandardCharsets.UTF_8);
    byte[] cid = contractId.getBytes(StandardCharsets.UTF_8);
    byte[] eid = endpointId.getBytes(StandardCharsets.UTF_8);
    byte[] hb = health.getBytes(StandardCharsets.UTF_8);
    long topicHandle = topic == null ? 0L : topic.handle();

    Arena arena = Arena.ofConfined();
    long opId;
    try {
      MemorySegment authorSeg = bytesToSegment(arena, author);
      MemorySegment svcSeg = bytesToSegment(arena, svc);
      MemorySegment cidSeg = bytesToSegment(arena, cid);
      MemorySegment eidSeg = bytesToSegment(arena, eid);
      MemorySegment hSeg = bytesToSegment(arena, hb);
      MemorySegment opSeg = arena.allocate(ValueLayout.JAVA_LONG);
      int status =
          lib.asterRegistryRenewLease(
              doc.runtime().nativeHandle(),
              doc.docHandle(),
              authorSeg,
              author.length,
              svcSeg,
              svc.length,
              cidSeg,
              cid.length,
              eidSeg,
              eid.length,
              hSeg,
              hb.length,
              load,
              leaseDurationS,
              topicHandle,
              0L,
              opSeg);
      if (status != 0) {
        arena.close();
        return CompletableFuture.failedFuture(
            new IrohException(
                IrohStatus.fromCode(status), "aster_registry_renew_lease: " + status));
      }
      opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      arena.close();
      return CompletableFuture.failedFuture(
          new IrohException("aster_registry_renew_lease threw: " + t.getMessage()));
    }
    return doc.runtime()
        .registry()
        .register(opId)
        .whenComplete((u, e) -> arena.close())
        .thenApply(
            event -> {
              if (event.kind() != IrohEventKind.REGISTRY_RENEWED) {
                throw new IrohException("renew_lease: unexpected event " + event.kind());
              }
              return null;
            });
  }

  /** Add an author to the per-doc registry ACL writer set. */
  public static CompletableFuture<Void> aclAddWriterAsync(
      Doc doc, String authorId, String writerId) {
    return mutateAclWriter(doc, authorId, writerId, true);
  }

  /** Remove an author from the per-doc registry ACL writer set. */
  public static CompletableFuture<Void> aclRemoveWriterAsync(
      Doc doc, String authorId, String writerId) {
    return mutateAclWriter(doc, authorId, writerId, false);
  }

  private static CompletableFuture<Void> mutateAclWriter(
      Doc doc, String authorId, String writerId, boolean add) {
    var lib = IrohLibrary.getInstance();
    byte[] author = authorId.getBytes(StandardCharsets.UTF_8);
    byte[] writer = writerId.getBytes(StandardCharsets.UTF_8);

    Arena arena = Arena.ofConfined();
    long opId;
    try {
      MemorySegment authorSeg = bytesToSegment(arena, author);
      MemorySegment writerSeg = bytesToSegment(arena, writer);
      MemorySegment opSeg = arena.allocate(ValueLayout.JAVA_LONG);
      int status =
          add
              ? lib.asterRegistryAclAddWriter(
                  doc.runtime().nativeHandle(),
                  doc.docHandle(),
                  authorSeg,
                  author.length,
                  writerSeg,
                  writer.length,
                  0L,
                  opSeg)
              : lib.asterRegistryAclRemoveWriter(
                  doc.runtime().nativeHandle(),
                  doc.docHandle(),
                  authorSeg,
                  author.length,
                  writerSeg,
                  writer.length,
                  0L,
                  opSeg);
      if (status != 0) {
        arena.close();
        return CompletableFuture.failedFuture(
            new IrohException(
                IrohStatus.fromCode(status),
                (add ? "aster_registry_acl_add_writer" : "aster_registry_acl_remove_writer")
                    + ": "
                    + status));
      }
      opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      arena.close();
      return CompletableFuture.failedFuture(
          new IrohException("acl mutate threw: " + t.getMessage()));
    }
    return doc.runtime()
        .registry()
        .register(opId)
        .whenComplete((u, e) -> arena.close())
        .thenApply(
            event -> {
              if (event.kind() != IrohEventKind.REGISTRY_ACL_UPDATED) {
                throw new IrohException("acl mutate: unexpected event " + event.kind());
              }
              return null;
            });
  }

  /** List the current writer set for the per-doc registry ACL. Empty when ACL is in open mode. */
  public static CompletableFuture<List<String>> aclListWritersAsync(Doc doc) {
    var lib = IrohLibrary.getInstance();
    Arena arena = Arena.ofConfined();
    long opId;
    try {
      MemorySegment opSeg = arena.allocate(ValueLayout.JAVA_LONG);
      int status =
          lib.asterRegistryAclListWriters(doc.runtime().nativeHandle(), doc.docHandle(), 0L, opSeg);
      if (status != 0) {
        arena.close();
        return CompletableFuture.failedFuture(
            new IrohException(
                IrohStatus.fromCode(status), "aster_registry_acl_list_writers: " + status));
      }
      opId = opSeg.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      arena.close();
      return CompletableFuture.failedFuture(
          new IrohException("aster_registry_acl_list_writers threw: " + t.getMessage()));
    }
    return doc.runtime()
        .registry()
        .register(opId)
        .whenComplete((u, e) -> arena.close())
        .thenApply(
            event -> {
              if (event.kind() != IrohEventKind.REGISTRY_ACL_LISTED) {
                throw new IrohException("acl list: unexpected event " + event.kind());
              }
              byte[] payload = event.data().asByteBuffer().array();
              if (payload.length == 0) {
                return List.<String>of();
              }
              try {
                return RegistryMapper.MAPPER.readValue(payload, STRING_LIST);
              } catch (java.io.IOException e) {
                throw new IrohException("acl list: failed to decode writers: " + e.getMessage());
              }
            });
  }

  private static MemorySegment bytesToSegment(Arena arena, byte[] data) {
    if (data.length == 0) {
      // Allocate a 1-byte segment so the resulting MemorySegment is non-NULL; the FFI side
      // checks the length and ignores the pointer when length is 0.
      return arena.allocate(1);
    }
    MemorySegment seg = arena.allocate(data.length);
    seg.copyFrom(MemorySegment.ofArray(data));
    return seg;
  }
}
