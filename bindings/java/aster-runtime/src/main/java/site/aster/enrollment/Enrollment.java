package site.aster.enrollment;

import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;
import site.aster.node.NodeAddr;
import site.aster.upcall.UpcallNative;

/**
 * The Enrollment a server or client installed from its {@link site.aster.config.AsterConfig}
 * ({@code AsterServer.enrollment()}, {@code AsterClient.enrollment()}; Rust {@code
 * aster::enrollment::Enrollment}; C ABI 5.1). Nothing here is needed for roles or revocation, which
 * apply automatically: these are escape hatches for audit, diagnostics and checks before a
 * sensitive operation. The node owns it; closing the server or client releases it.
 */
public final class Enrollment implements AutoCloseable {

  /** The ALPN of the reciprocal enrollment handshake. */
  public static final String ALPN = "aster.enrollment/1";

  static final ObjectMapper RESULTS =
      new ObjectMapper()
          .setPropertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE)
          .configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

  private static final ValueLayout.OfInt I = ValueLayout.JAVA_INT;
  private static final ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
  private static final java.lang.foreign.AddressLayout A = ValueLayout.ADDRESS;
  private static final java.lang.foreign.MemoryLayout B = IrohLibrary.ASTER_BYTES;

  private static final MethodHandle ENROLL =
      UpcallNative.handle("aster_enrollment_enroll", FunctionDescriptor.of(I, J, J, B, J, A));
  private static final MethodHandle FREE =
      UpcallNative.handle("aster_enrollment_free", FunctionDescriptor.of(I, J, J));
  private static final MethodHandle CHECK_PEER =
      UpcallNative.handle("aster_enrollment_check_peer", FunctionDescriptor.of(I, J, J, B, J, A));
  private static final MethodHandle PEER_CLAIMS =
      UpcallNative.handle("aster_enrollment_peer_claims", FunctionDescriptor.of(I, J, J, B, A));
  private static final MethodHandle STATUS =
      UpcallNative.handle("aster_enrollment_status", FunctionDescriptor.of(I, J, J, A));
  private static final MethodHandle NEXT_REVOCATION =
      UpcallNative.handle("aster_enrollment_next_revocation", FunctionDescriptor.of(I, J, J, J, A));
  private static final MethodHandle ISSUER_INFO =
      UpcallNative.handle("aster_enrollment_issuer_info", FunctionDescriptor.of(I, J, J, A));
  private static final MethodHandle ISSUE =
      UpcallNative.handle("aster_enrollment_issue", FunctionDescriptor.of(I, J, J, B, A));
  private static final MethodHandle MINT_USER_TOKEN =
      UpcallNative.handle("aster_enrollment_mint_user_token", FunctionDescriptor.of(I, J, J, B, A));
  private static final MethodHandle PUBLISHER_NOTICE =
      UpcallNative.handle(
          "aster_enrollment_publisher_notice", FunctionDescriptor.of(I, J, J, B, B));
  private static final MethodHandle PUBLISHER_NOTICE_TOKEN =
      UpcallNative.handle(
          "aster_enrollment_publisher_notice_token", FunctionDescriptor.of(I, J, J, B));
  private static final MethodHandle PUBLISHER_STATUS =
      UpcallNative.handle("aster_enrollment_publisher_status", FunctionDescriptor.of(I, J, J, A));
  private static final MethodHandle STRING_RELEASE =
      UpcallNative.handle("aster_string_release", FunctionDescriptor.of(I, A, J));

  private final IrohRuntime runtime;
  private final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();

  /** Servers this client enrolled with (node id → in flight or done). */
  private final Map<String, CompletableFuture<Void>> enrolled = new ConcurrentHashMap<>();

  /** Wrap the {@code aster_enrollment_t} a node start returned; binding plumbing. */
  public Enrollment(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  /** The native {@code aster_enrollment_t}. */
  public long nativeHandle() {
    return handle;
  }

  private void live() {
    if (closed.get()) {
      throw new IrohException(IrohStatus.ALREADY_CLOSED, "enrollment closed");
    }
  }

  /**
   * Release the enrollment early (closing the node does it too): pending {@link #nextRevocation}
   * waits end with {@code ALREADY_CLOSED} and a client stops renewing.
   */
  @Override
  public void close() {
    if (closed.compareAndSet(false, true)) {
      try {
        FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime closed: the node released it
      }
    }
  }

  /** Mark the enrollment released by its node (no native call). */
  public void markReleased() {
    closed.set(true);
  }

  // ── client sequence ─────────────────────────────────────────────────────

  /**
   * The client sequence against {@code server} (design §15.7): the reciprocal handshake (the server
   * verifies this node, this node verifies the server), synchronization of the fleet namespace the
   * server offers, and renewal from that server when it signed this node's credential (or from
   * {@code renewFrom}). {@code AsterClient} runs it before its first connection to each server; it
   * runs once per server (a failed attempt is retried on the next call).
   */
  public CompletableFuture<Void> enroll(NodeAddr server) {
    String id = server.endpointId();
    CompletableFuture<Void> fresh = new CompletableFuture<>();
    CompletableFuture<Void> prior = enrolled.putIfAbsent(id, fresh);
    if (prior != null) {
      return prior;
    }
    Map<String, Object> addr = new LinkedHashMap<>();
    addr.put("node_id", id);
    if (server.relayUrl() != null && !server.relayUrl().isEmpty()) {
      addr.put("relay_url", server.relayUrl());
    }
    addr.put(
        "direct_addresses",
        server.directAddresses() == null ? List.of() : server.directAddresses());
    byte[] body = json(addr);
    run(
            "aster_enrollment_enroll",
            (a, op) ->
                (int)
                    ENROLL.invoke(runtime.nativeHandle(), handle, DataPlane.bytes(a, body), 0L, op))
        .whenComplete(
            (r, err) -> {
              if (err != null) {
                enrolled.remove(id, fresh);
                fresh.completeExceptionally(err);
              } else {
                fresh.complete(null);
              }
            });
    return fresh;
  }

  // ── escape hatches ──────────────────────────────────────────────────────

  /**
   * Re-evaluate an admitted peer against current policy (call it before a sensitive operation).
   * Fails with an {@link AdmissionException} saying why not; only {@link
   * AdmissionException.Kind#DENIED} revokes the session.
   */
  public CompletableFuture<Readiness> checkPeer(String peer) {
    return run(
            "aster_enrollment_check_peer",
            (a, op) ->
                (int)
                    CHECK_PEER.invoke(
                        runtime.nativeHandle(), handle, DataPlane.utf8(a, peer), 0L, op))
        .thenApply(b -> decode(b, Readiness.class));
  }

  /**
   * The verified evidence {@code peer}'s admission session rests on (subject, issuer path, scopes,
   * tags, attributes, validity), for audit; empty when the peer has no session (never enrolled,
   * revoked, evicted). A server already maps it to roles on every call ({@code
   * [enrollment.roles]}): do not copy it into peer attributes.
   */
  public Optional<PeerEvidence> peerClaims(String peer) {
    live();
    try (Arena a = Arena.ofConfined()) {
      MemorySegment out = a.allocate(B);
      int status =
          (int) PEER_CLAIMS.invoke(runtime.nativeHandle(), handle, DataPlane.utf8(a, peer), out);
      if (status == IrohStatus.NOT_FOUND.code) {
        return Optional.empty();
      }
      UpcallNative.check(status, "aster_enrollment_peer_claims");
      return Optional.of(decode(take(out), PeerEvidence.class));
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /** This node's Enrollment state. */
  public EnrollmentStatus status() {
    return decode(
        sync("aster_enrollment_status", (a, out) -> (int) STATUS.invoke(rt(), handle, out)),
        EnrollmentStatus.class);
  }

  /** This node's current credential evidence (renewal replaces it). */
  public PeerEvidence credential() {
    return status().credential();
  }

  /**
   * Wait for the next revocation of a formerly admitted peer (buffered from start). Cancelling the
   * future consumes nothing; {@link #close} ends a pending wait with {@code ALREADY_CLOSED}. Roles
   * and connections need no action: both are already gone. Use it for application state keyed by
   * peer.
   */
  public CompletableFuture<Revocation> nextRevocation() {
    return run(
            "aster_enrollment_next_revocation",
            (a, op) -> (int) NEXT_REVOCATION.invoke(rt(), handle, 0L, op))
        .thenApply(b -> decode(b, Revocation.class));
  }

  // ── gateway identity (C ABI 7.7) ────────────────────────────────────────

  /**
   * Mint a short-lived token for user {@code sub} with {@code roles}, signed in by {@code amr} (at
   * least one, e.g. {@link site.aster.identity.GatewayIdentity#AMR_WEBAUTHN}), valid for {@code
   * ttl} (1 s to 5 minutes, rounded up to whole seconds), signed with this node's key (Rust {@code
   * AsterClient::user_token_issuer} + {@code Issuer::mint_user_token}). The roles must be within
   * the issuer delegation ({@link site.aster.identity.GatewayIdentity#ISSUABLE_ROLES_ATTRIBUTE}) in
   * the credential held now; anything else is {@code INVALID_ARGUMENT}. Attach it with {@code
   * CallOptions.withOnBehalfOf}; this node needs {@code aster.gateway}.
   */
  public String mintUserToken(String sub, List<String> roles, List<String> amr, Duration ttl) {
    live();
    java.util.Objects.requireNonNull(ttl, "ttl");
    long secs = ttl.getSeconds() + (ttl.getNano() > 0 ? 1 : 0);
    Map<String, Object> req = new LinkedHashMap<>();
    req.put("sub", sub);
    req.put("roles", roles == null ? List.of() : roles);
    req.put("amr", amr == null ? List.of() : amr);
    req.put("ttl_secs", Math.max(0, secs));
    byte[] body = json(req);
    return new String(
        sync(
            "aster_enrollment_mint_user_token",
            (a, out) -> (int) MINT_USER_TOKEN.invoke(rt(), handle, DataPlane.bytes(a, body), out)),
        StandardCharsets.UTF_8);
  }

  /**
   * Tell this node's issuer-key publisher ({@code [identity.publisher]}) that a token of {@code
   * iss} named {@code kid}: an unknown kid refreshes {@code iss} at once (at most every {@code
   * min_refresh_secs}). Rust {@code PublisherHandle::notice_kid}.
   *
   * @throws IrohException {@code NOT_FOUND} when the node runs no publisher
   */
  public void noticeKid(String iss, String kid) {
    live();
    try (Arena a = Arena.ofConfined()) {
      UpcallNative.check(
          (int)
              PUBLISHER_NOTICE.invoke(rt(), handle, DataPlane.utf8(a, iss), DataPlane.utf8(a, kid)),
          "aster_enrollment_publisher_notice");
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * {@link #noticeKid} with the {@code iss} and {@code kid} of a compact JWT (unverified; a token
   * that does not parse is ignored). {@code AsterClient} calls it for every token it forwards.
   *
   * @throws IrohException {@code NOT_FOUND} when the node runs no publisher
   */
  public void noticeToken(String token) {
    live();
    try (Arena a = Arena.ofConfined()) {
      UpcallNative.check(
          (int) PUBLISHER_NOTICE_TOKEN.invoke(rt(), handle, DataPlane.utf8(a, token)),
          "aster_enrollment_publisher_notice_token");
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /**
   * The issuer-key publisher's state per issuer.
   *
   * @throws IrohException {@code NOT_FOUND} when the node runs no publisher
   */
  public Map<String, site.aster.identity.IssuerPublishStatus> publisherStatus() {
    byte[] json =
        sync(
            "aster_enrollment_publisher_status",
            (a, out) -> (int) PUBLISHER_STATUS.invoke(rt(), handle, out));
    return site.aster.identity.IdentityJson.decode(
        json,
        new com.fasterxml.jackson.core.type.TypeReference<
            Map<String, site.aster.identity.IssuerPublishStatus>>() {},
        "publisher status");
  }

  // ── issuer (package-private; EnrollmentIssuer) ──────────────────────────

  IssuerInfo issuerInfo() {
    return decode(
        sync(
            "aster_enrollment_issuer_info",
            (a, out) -> (int) ISSUER_INFO.invoke(rt(), handle, out)),
        IssuerInfo.class);
  }

  IssuedCredential issue(String subject, EnrollmentClaims claims, Registration registration) {
    live();
    Map<String, Object> req = new LinkedHashMap<>();
    req.put("subject", subject);
    Map<String, Object> c = new LinkedHashMap<>();
    if (claims != null) {
      c.put("scopes", claims.scopes());
      c.put("tags", claims.tags());
      c.put("attributes", claims.attributes());
    }
    req.put("claims", c);
    req.put("registration", (registration == null ? Registration.EPHEMERAL : registration).wire());
    byte[] body = json(req);
    try (Arena a = Arena.ofConfined()) {
      MemorySegment out = a.allocate(B);
      int status = (int) ISSUE.invoke(rt(), handle, DataPlane.bytes(a, body), out);
      if (status == IrohStatus.PERMISSION_DENIED.code
          && !out.get(A, 0).equals(MemorySegment.NULL)) {
        var refusal = RESULTS.readTree(take(out));
        throw new IssuerRefusedException(
            refusal.path("code").asText(),
            refusal.path("message").asText(),
            refusal.path("retryable").asBoolean());
      }
      UpcallNative.check(status, "aster_enrollment_issue");
      return decode(take(out), IssuedCredential.class);
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  // ── plumbing ─────────────────────────────────────────────────────────────

  private long rt() {
    return runtime.nativeHandle();
  }

  /** An async operation completing with {@code ENROLLMENT_RESULT}: its JSON payload. */
  private CompletableFuture<byte[]> run(String name, UpcallNative.OutCall call) {
    try {
      live();
    } catch (IrohException e) {
      return CompletableFuture.failedFuture(e);
    }
    return UpcallNative.submit(runtime, name, call)
        .handle(
            (r, err) -> {
              if (err != null) {
                Throwable cause =
                    err instanceof CompletionException && err.getCause() != null
                        ? err.getCause()
                        : err;
                if (cause instanceof IrohException ie) {
                  AdmissionException.Kind kind = AdmissionException.Kind.fromFlags(ie.eventFlags());
                  if (kind != AdmissionException.Kind.OTHER) {
                    throw new AdmissionException(kind, ie);
                  }
                  throw ie;
                }
                throw new CompletionException(cause);
              }
              if (r.event().kind() != IrohEventKind.ENROLLMENT_RESULT) {
                throw new IrohException(name + ": unexpected event " + r.event().kind());
              }
              return r.data();
            });
  }

  /** A synchronous call writing owned JSON to its last ({@code aster_bytes_t *}) argument. */
  private byte[] sync(String name, UpcallNative.OutCall call) {
    live();
    try (Arena a = Arena.ofConfined()) {
      MemorySegment out = a.allocate(B);
      UpcallNative.check(call.invoke(a, out), name);
      return take(out);
    } catch (Throwable t) {
      throw UpcallNative.rethrow(t);
    }
  }

  /** Copy and release an owned {@code aster_bytes_t}. */
  static byte[] take(MemorySegment bytes) throws Throwable {
    MemorySegment ptr = bytes.get(A, 0);
    long len = bytes.get(J, 8);
    if (ptr.equals(MemorySegment.NULL) || len == 0) {
      return new byte[0];
    }
    byte[] data = ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE);
    STRING_RELEASE.invoke(ptr, len);
    return data;
  }

  static <T> T decode(byte[] json, Class<T> type) {
    try {
      return RESULTS.readValue(json, type);
    } catch (java.io.IOException e) {
      throw new IrohException(
          "enrollment: malformed native result for "
              + type.getSimpleName()
              + ": "
              + new String(json, StandardCharsets.UTF_8));
    }
  }

  static byte[] json(Object value) {
    try {
      return TrustJson.MAPPER.writeValueAsBytes(value);
    } catch (com.fasterxml.jackson.core.JsonProcessingException e) {
      throw new IllegalArgumentException(e);
    }
  }
}
