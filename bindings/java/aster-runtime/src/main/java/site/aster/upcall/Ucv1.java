package site.aster.upcall;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import site.aster.lease.DenyReason;
import site.aster.lease.Fence;
import site.aster.lease.FenceRejection;
import site.aster.lease.LeaseOp;
import site.aster.lease.LeaseSnapshot;
import site.aster.lease.MutateResult;
import site.aster.lease.TransitionOutcome;

/**
 * UCv1, the canonical upcall payload encoding (design {@code
 * docs/_internal/bindings/upcall-design.md} §3, vectors {@code conformance/vectors/upcalls.json}).
 *
 * <pre>
 * message := tag:u8 0x01 field*
 * uint    := LEB128 varint, minimal length
 * u8enum  := one raw byte
 * bool    := 0x00 | 0x01
 * string  := varint((utf8_len &lt;&lt; 2) | 2) utf8      (minimal header)
 * bytes   := varint(len) raw
 * opt&lt;T&gt;  := 0xFD (absent) | 0x00 T
 * list&lt;T&gt; := varint(count) 0x0C T{count}
 * </pre>
 *
 * Every field is present in a fixed order with no trailing bytes; decoders refuse anything else, so
 * each value has exactly one encoding. Covers every message the C ABI issues or answers (leases,
 * fenced mutation, decisions, topology) plus expose / admission; placement ({@code BC}..{@code BF})
 * is not implemented (the C ABI has no coordination client).
 */
public final class Ucv1 {

  private Ucv1() {}

  public static final int VERSION = 0x01;

  /** Message tags. */
  public static final int TAG_LEASE_TRANSITION_REQUEST = 0xB1;

  public static final int TAG_TRANSITION_OUTCOME = 0xB2;
  public static final int TAG_LEASE_SNAPSHOT_REQUEST = 0xB3;
  public static final int TAG_LEASE_SNAPSHOT = 0xB4;
  public static final int TAG_LEASE_AUTHORIZE_REQUEST = 0xB5;
  public static final int TAG_FENCED_MUTATE_REQUEST = 0xB6;
  public static final int TAG_FENCED_MUTATE_RESPONSE = 0xB7;
  public static final int TAG_TOPOLOGY_ADMITTED_REQUEST = 0xB8;
  public static final int TAG_TOPOLOGY_ADMITTED_RESPONSE = 0xB9;
  public static final int TAG_TOPOLOGY_SNAPSHOT_REQUEST = 0xBA;
  public static final int TAG_TOPOLOGY_SNAPSHOT = 0xBB;
  public static final int TAG_EXPOSE_AUTHORIZE_REQUEST = 0xC0;
  public static final int TAG_ADMISSION_CONNECT_REQUEST = 0xC1;
  public static final int TAG_ADMISSION_HANDSHAKE_REQUEST = 0xC2;
  public static final int TAG_ADMISSION_HANDSHAKE_RESPONSE = 0xC3;
  public static final int TAG_EXPOSE_HTTP_REQUEST = 0xC4;
  public static final int TAG_EXPOSE_HTTP_RESPONSE = 0xC5;
  public static final int TAG_EXPOSE_HTTP_TRAILERS = 0xC6;
  public static final int TAG_EXPOSE_ROUTE_POLICY_REQUEST = 0xC7;
  public static final int TAG_EXPOSE_ROUTE_GRANT = 0xC8;
  public static final int TAG_RPC_AUTH_REQUEST = 0xC9;
  public static final int TAG_RPC_AUTH_RESPONSE = 0xCA;

  /** Highest HTTP version code ({@code 0} HTTP/0.9 .. {@code 4} HTTP/3). */
  public static final int HTTP_VERSION_MAX = 4;

  /** Route protocol codes of {@link ExposeRouteSpec#protocol()}. */
  public static final int ROUTE_PROTOCOL_HTTP = 0;

  public static final int ROUTE_PROTOCOL_TCP = 1;
  public static final int TAG_DECISION = 0xCF;

  private static final int NULL_FLAG = 0xFD;
  private static final int LIST_HEADER = 0x0C;

  /** A payload that is not the canonical encoding of the expected message. */
  public static final class DecodeException extends IllegalArgumentException {
    private static final long serialVersionUID = 1L;

    public DecodeException(String message) {
      super("UCv1: " + message);
    }
  }

  // ── message types without a home in site.aster.lease ─────────────────────

  public record LeaseTransitionRequest(String resource, LeaseOp op) {}

  /** Per-call authentication context. Map keys have canonical UTF-8 byte order on the wire. */
  public record RpcAuthRequest(
      String peerId, String service, String method, int sessionId, Map<String, String> metadata) {
    public RpcAuthRequest {
      metadata = Map.copyOf(metadata);
    }
  }

  /** Explicit rejection or a nonempty resolved principal with per-call attributes. */
  public record RpcAuthResponse(
      boolean accepted, String principal, Map<String, String> attributes) {
    public RpcAuthResponse {
      attributes = Map.copyOf(attributes);
    }

    public static RpcAuthResponse rejected() {
      return new RpcAuthResponse(false, null, Map.of());
    }
  }

  private static byte[] authUtf8(String value) {
    try {
      ByteBuffer encoded =
          StandardCharsets.UTF_8
              .newEncoder()
              .onMalformedInput(CodingErrorAction.REPORT)
              .onUnmappableCharacter(CodingErrorAction.REPORT)
              .encode(CharBuffer.wrap(value));
      byte[] bytes = new byte[encoded.remaining()];
      encoded.get(bytes);
      return bytes;
    } catch (CharacterCodingException e) {
      throw new IllegalArgumentException("authentication string is not valid Unicode", e);
    }
  }

  private static W authString(W w, String value) {
    authUtf8(value);
    return w.str(value);
  }

  private static void putAuthMap(W w, Map<String, String> values) {
    List<String> keys = new ArrayList<>(values.keySet());
    keys.sort((a, b) -> Arrays.compareUnsigned(authUtf8(a), authUtf8(b)));
    w.list(keys.size());
    for (String key : keys) {
      authString(w, key);
      authString(w, values.get(key));
    }
  }

  private static Map<String, String> getAuthMap(R r) {
    int count = r.list();
    Map<String, String> values = new LinkedHashMap<>();
    byte[] previous = null;
    for (int i = 0; i < count; i++) {
      String key = r.str();
      byte[] encoded = authUtf8(key);
      if (previous != null && Arrays.compareUnsigned(previous, encoded) >= 0)
        throw new DecodeException("authentication map keys not strictly ascending");
      previous = encoded;
      values.put(key, r.str());
    }
    return Map.copyOf(values);
  }

  public static byte[] encodeRpcAuthRequest(RpcAuthRequest value) {
    W w = new W(TAG_RPC_AUTH_REQUEST);
    authString(w, value.peerId());
    authString(w, value.service());
    authString(w, value.method());
    w.uint(Integer.toUnsignedLong((value.sessionId() << 1) ^ (value.sessionId() >> 31)));
    putAuthMap(w, value.metadata());
    return w.done();
  }

  public static RpcAuthRequest decodeRpcAuthRequest(byte[] bytes) {
    R r = new R(bytes, TAG_RPC_AUTH_REQUEST);
    String peer = r.str(), service = r.str(), method = r.str();
    int raw = (int) r.uintMax(0xffff_ffffL, "authentication session id");
    int session = (raw >>> 1) ^ -(raw & 1);
    var value = new RpcAuthRequest(peer, service, method, session, getAuthMap(r));
    r.end();
    return value;
  }

  public static byte[] encodeRpcAuthResponse(RpcAuthResponse value) {
    W w = new W(TAG_RPC_AUTH_RESPONSE).bool(value.accepted());
    if (value.accepted()) {
      if (value.principal() == null || value.principal().isEmpty())
        throw new IllegalArgumentException("empty authentication principal");
      authString(w, value.principal());
      putAuthMap(w, value.attributes());
    } else if (value.principal() != null || !value.attributes().isEmpty()) {
      throw new IllegalArgumentException("rejected authentication carries fields");
    }
    return w.done();
  }

  public static RpcAuthResponse decodeRpcAuthResponse(byte[] bytes) {
    R r = new R(bytes, TAG_RPC_AUTH_RESPONSE);
    RpcAuthResponse value;
    if (r.bool()) {
      String principal = r.str();
      if (principal.isEmpty()) throw new DecodeException("empty authentication principal");
      value = new RpcAuthResponse(true, principal, getAuthMap(r));
    } else value = RpcAuthResponse.rejected();
    r.end();
    return value;
  }

  public record FencedMutateRequest(Fence fence, String writer, byte[] op) {
    @Override
    public boolean equals(Object o) {
      return o instanceof FencedMutateRequest r
          && fence.equals(r.fence)
          && writer.equals(r.writer)
          && Arrays.equals(op, r.op);
    }

    @Override
    public int hashCode() {
      return fence.hashCode() * 31 + Arrays.hashCode(op);
    }
  }

  public record ClusterView(List<String> members, String bridge, List<String> witnesses) {}

  public record FarPair(String a, String b) {}

  /** A serialized shared topology view (what {@code SharedView::from_parts} takes). */
  public record TopologySnapshot(
      List<ClusterView> clusters, List<FarPair> farPairs, long separationCoverage) {}

  public record ExposeAuthorizeRequest(String serviceId, String peerId, byte[] metadata) {}

  /** One HTTP header; a repeated name is one entry per value. {@code name} is lowercase. */
  public record HttpHeader(String name, byte[] value) {}

  /** Where an HTTP message body travels (R9). */
  public sealed interface HttpBody {
    /** The whole body is in the message (possibly empty). */
    record Inline(byte[] body) implements HttpBody {}

    /** The body streams through the exchange's body handle ({@code stream_id}). */
    record Stream() implements HttpBody {}

    HttpBody STREAM = new Stream();
  }

  /**
   * Kind 49 {@code expose_http} request. {@code peerId} is {@code null} when the request did not
   * come over the relay.
   */
  public record ExposeHttpRequest(
      String serviceId,
      String peerId,
      long streamId,
      int version,
      String method,
      String uri,
      List<HttpHeader> headers,
      HttpBody body) {}

  /** Kind 49 {@code expose_http} response head; {@code status} is 100..=999. */
  public record ExposeHttpResponse(int status, List<HttpHeader> headers, HttpBody body) {}

  /** A route spec as an edge's route policy sees it. */
  public record ExposeRouteSpec(String host, int port, int protocol, String serviceId) {}

  /** Kind 50 {@code expose_route_policy} request; answered with a route grant. */
  public record ExposeRoutePolicyRequest(
      String peerId, byte[] metadata, List<ExposeRouteSpec> specs) {}

  public record AdmissionConnectRequest(String peer, byte[] alpn) {}

  public record AdmissionHandshakeRequest(String peer, byte[] alpn, boolean isAlive) {}

  /** Accept ({@code accept}, code 0, empty reason) or reject with a code and reason. */
  public record HandshakeDecision(boolean accept, long code, byte[] reason) {
    public static HandshakeDecision accepted() {
      return new HandshakeDecision(true, 0, new byte[0]);
    }
  }

  // ── writer / reader ──────────────────────────────────────────────────────

  /** UCv1 message writer (also used by {@code site.aster.coordination}). */
  public static final class W {
    private final ByteArrayOutputStream out = new ByteArrayOutputStream();

    public W(int tag) {
      out.write(tag);
      out.write(VERSION);
    }

    public W u8(int v) {
      out.write(v & 0xff);
      return this;
    }

    public W uint(long v) {
      long x = v;
      while ((x & ~0x7FL) != 0) {
        out.write((int) ((x & 0x7F) | 0x80));
        x >>>= 7;
      }
      out.write((int) x);
      return this;
    }

    public W bool(boolean v) {
      return u8(v ? 1 : 0);
    }

    public W str(String s) {
      byte[] b = s.getBytes(StandardCharsets.UTF_8);
      uint(((long) b.length << 2) | 2);
      out.writeBytes(b);
      return this;
    }

    public W bytes(byte[] b) {
      uint(b.length);
      out.writeBytes(b);
      return this;
    }

    public W list(int n) {
      uint(n);
      return u8(LIST_HEADER);
    }

    public W absent() {
      return u8(NULL_FLAG);
    }

    public W present() {
      return u8(0);
    }

    public W strs(List<String> v) {
      list(v.size());
      for (String s : v) {
        str(s);
      }
      return this;
    }

    public byte[] done() {
      return out.toByteArray();
    }
  }

  /** UCv1 message reader. */
  public static final class R {
    private final byte[] buf;
    private int pos;

    public R(byte[] buf, int tag) {
      this.buf = buf;
      if (buf.length == 0) {
        throw new DecodeException("empty payload");
      }
      if ((buf[0] & 0xff) != tag) {
        throw new DecodeException(String.format("tag 0x%02x, expected 0x%02x", buf[0] & 0xff, tag));
      }
      if (buf.length < 2) {
        throw new DecodeException("missing version");
      }
      if ((buf[1] & 0xff) != VERSION) {
        throw new DecodeException(String.format("unsupported version 0x%02x", buf[1] & 0xff));
      }
      pos = 2;
    }

    private void need(int n) {
      if (n < 0 || buf.length - pos < n) {
        throw new DecodeException("unexpected EOF at offset " + pos);
      }
    }

    public int u8() {
      need(1);
      return buf[pos++] & 0xff;
    }

    /** Unsigned LEB128, minimal, at most 64 bits (returned as a two's-complement long). */
    public long uint() {
      int start = pos;
      long v = 0;
      for (int i = 0; i < 10; i++) {
        int b = u8();
        if (i == 9 && b > 1) {
          throw new DecodeException("varint overflows 64 bits at offset " + start);
        }
        v |= (long) (b & 0x7F) << (7 * i);
        if ((b & 0x80) == 0) {
          if (i > 0 && b == 0) {
            throw new DecodeException("non-minimal varint at offset " + start);
          }
          return v;
        }
      }
      throw new DecodeException("varint too long at offset " + start);
    }

    public long uintMax(long max, String what) {
      long v = uint();
      if (Long.compareUnsigned(v, max) > 0) {
        throw new DecodeException(what + " " + Long.toUnsignedString(v) + " out of range");
      }
      return v;
    }

    public boolean bool() {
      int b = u8();
      return switch (b) {
        case 0 -> false;
        case 1 -> true;
        default -> throw new DecodeException("invalid bool byte " + b);
      };
    }

    public String str() {
      int start = pos;
      long header = uint();
      if ((header & 0b11) != 0b10) {
        throw new DecodeException("string header has wrong encoding bits at offset " + start);
      }
      long len = header >>> 2;
      if (len > Integer.MAX_VALUE) {
        throw new DecodeException("string too long");
      }
      need((int) len);
      try {
        String s =
            StandardCharsets.UTF_8
                .newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(buf, pos, (int) len))
                .toString();
        pos += (int) len;
        return s;
      } catch (CharacterCodingException e) {
        throw new DecodeException("invalid UTF-8 at offset " + start);
      }
    }

    public byte[] bytes() {
      long len = uint();
      if (len > buf.length - pos) {
        throw new DecodeException("bytes length " + len + " exceeds payload");
      }
      byte[] b = Arrays.copyOfRange(buf, pos, pos + (int) len);
      pos += (int) len;
      return b;
    }

    public int list() {
      long n = uint();
      if (u8() != LIST_HEADER) {
        throw new DecodeException("list element header mismatch");
      }
      if (n > buf.length - pos) {
        throw new DecodeException("list count " + n + " exceeds payload");
      }
      return (int) n;
    }

    public boolean present() {
      int b = u8();
      if (b == 0) {
        return true;
      }
      if (b == NULL_FLAG) {
        return false;
      }
      throw new DecodeException("invalid optional marker " + b);
    }

    public List<String> strs() {
      int n = list();
      List<String> out = new ArrayList<>(n);
      for (int i = 0; i < n; i++) {
        out.add(str());
      }
      return List.copyOf(out);
    }

    public void end() {
      if (pos != buf.length) {
        throw new DecodeException((buf.length - pos) + " trailing bytes");
      }
    }
  }

  // ── leases ───────────────────────────────────────────────────────────────

  private static void putOp(W w, LeaseOp op) {
    switch (op) {
      case LeaseOp.Acquire a -> w.u8(1).str(a.candidate()).uint(a.ttlMs());
      case LeaseOp.Renew r -> w.u8(2).str(r.holder()).uint(r.epoch());
      case LeaseOp.Release r -> w.u8(3).str(r.holder()).uint(r.epoch());
      case LeaseOp.Revoke r -> w.u8(4).str(r.authority());
    }
  }

  private static LeaseOp getOp(R r) {
    int op = r.u8();
    return switch (op) {
      case 1 -> new LeaseOp.Acquire(r.str(), r.uint());
      case 2 -> new LeaseOp.Renew(r.str(), r.uint());
      case 3 -> new LeaseOp.Release(r.str(), r.uint());
      case 4 -> new LeaseOp.Revoke(r.str());
      default -> throw new DecodeException("unknown lease op " + op);
    };
  }

  private static void putFence(W w, Fence f) {
    w.str(f.resource()).uint(f.epoch()).str(f.holder());
  }

  private static Fence getFence(R r) {
    return new Fence(r.str(), r.uint(), r.str());
  }

  private static void putSnapshot(W w, LeaseSnapshot s) {
    w.str(s.resource()).uint(s.epoch());
    if (s.holder() == null) {
      w.absent();
    } else {
      w.present().str(s.holder());
    }
    w.bool(s.held());
    if (s.remainingMs() == null) {
      w.absent();
    } else {
      w.present().uint(s.remainingMs());
    }
  }

  private static LeaseSnapshot getSnapshot(R r) {
    String resource = r.str();
    long epoch = r.uint();
    String holder = r.present() ? r.str() : null;
    boolean held = r.bool();
    Long remaining = r.present() ? r.uint() : null;
    return new LeaseSnapshot(resource, epoch, holder, held, remaining);
  }

  public static byte[] encodeLeaseTransitionRequest(String resource, LeaseOp op) {
    W w = new W(TAG_LEASE_TRANSITION_REQUEST).str(resource);
    putOp(w, op);
    return w.done();
  }

  public static LeaseTransitionRequest decodeLeaseTransitionRequest(byte[] buf) {
    R r = new R(buf, TAG_LEASE_TRANSITION_REQUEST);
    LeaseTransitionRequest v = new LeaseTransitionRequest(r.str(), getOp(r));
    r.end();
    return v;
  }

  public static byte[] encodeTransitionOutcome(TransitionOutcome o) {
    W w = new W(TAG_TRANSITION_OUTCOME);
    switch (o) {
      case TransitionOutcome.Granted g -> {
        w.u8(1);
        putFence(w, g.fence());
      }
      case TransitionOutcome.Renewed x -> w.u8(2);
      case TransitionOutcome.Released x -> w.u8(3);
      case TransitionOutcome.Revoked x -> w.u8(4);
      case TransitionOutcome.Denied d -> {
        w.u8(5).u8(d.reason().code);
        putSnapshot(w, d.snapshot());
      }
    }
    return w.done();
  }

  public static TransitionOutcome decodeTransitionOutcome(byte[] buf) {
    R r = new R(buf, TAG_TRANSITION_OUTCOME);
    int kind = r.u8();
    TransitionOutcome out =
        switch (kind) {
          case 1 -> new TransitionOutcome.Granted(getFence(r));
          case 2 -> TransitionOutcome.RENEWED;
          case 3 -> TransitionOutcome.RELEASED;
          case 4 -> TransitionOutcome.REVOKED;
          case 5 -> {
            int code = r.u8();
            DenyReason reason = DenyReason.fromCode(code);
            if (reason == null) {
              throw new DecodeException("unknown deny reason " + code);
            }
            yield new TransitionOutcome.Denied(reason, getSnapshot(r));
          }
          default -> throw new DecodeException("unknown transition outcome " + kind);
        };
    r.end();
    return out;
  }

  public static byte[] encodeLeaseSnapshotRequest(String resource) {
    return new W(TAG_LEASE_SNAPSHOT_REQUEST).str(resource).done();
  }

  public static String decodeLeaseSnapshotRequest(byte[] buf) {
    R r = new R(buf, TAG_LEASE_SNAPSHOT_REQUEST);
    String s = r.str();
    r.end();
    return s;
  }

  public static byte[] encodeLeaseSnapshot(LeaseSnapshot s) {
    W w = new W(TAG_LEASE_SNAPSHOT);
    putSnapshot(w, s);
    return w.done();
  }

  public static LeaseSnapshot decodeLeaseSnapshot(byte[] buf) {
    R r = new R(buf, TAG_LEASE_SNAPSHOT);
    LeaseSnapshot s = getSnapshot(r);
    r.end();
    return s;
  }

  public static byte[] encodeLeaseAuthorizeRequest(LeaseOp op) {
    W w = new W(TAG_LEASE_AUTHORIZE_REQUEST);
    putOp(w, op);
    return w.done();
  }

  public static LeaseOp decodeLeaseAuthorizeRequest(byte[] buf) {
    R r = new R(buf, TAG_LEASE_AUTHORIZE_REQUEST);
    LeaseOp op = getOp(r);
    r.end();
    return op;
  }

  public static byte[] encodeFencedMutateRequest(Fence fence, String writer, byte[] op) {
    W w = new W(TAG_FENCED_MUTATE_REQUEST);
    putFence(w, fence);
    return w.str(writer).bytes(op).done();
  }

  public static FencedMutateRequest decodeFencedMutateRequest(byte[] buf) {
    R r = new R(buf, TAG_FENCED_MUTATE_REQUEST);
    FencedMutateRequest v = new FencedMutateRequest(getFence(r), r.str(), r.bytes());
    r.end();
    return v;
  }

  public static byte[] encodeFencedMutateResponse(MutateResult res) {
    W w = new W(TAG_FENCED_MUTATE_RESPONSE);
    switch (res) {
      case MutateResult.Applied a -> w.u8(0).bytes(a.output());
      case MutateResult.Rejected x -> {
        FenceRejection rej = x.rejection();
        boolean epochs =
            rej.reason() == FenceRejection.Reason.STALE_EPOCH
                || rej.reason() == FenceRejection.Reason.FUTURE_EPOCH;
        w.u8(1)
            .u8(rej.reason().code)
            .uint(epochs ? rej.carried() : 0)
            .uint(epochs ? rej.current() : 0);
      }
    }
    return w.done();
  }

  public static MutateResult decodeFencedMutateResponse(byte[] buf) {
    R r = new R(buf, TAG_FENCED_MUTATE_RESPONSE);
    int kind = r.u8();
    MutateResult out;
    if (kind == 0) {
      out = new MutateResult.Applied(r.bytes());
    } else if (kind == 1) {
      int code = r.u8();
      long carried = r.uint();
      long current = r.uint();
      FenceRejection.Reason reason = FenceRejection.Reason.fromCode(code);
      if (reason == null) {
        throw new DecodeException("unknown fence rejection " + code);
      }
      if ((reason == FenceRejection.Reason.NOT_HOLDER || reason == FenceRejection.Reason.NOT_HELD)
          && (carried != 0 || current != 0)) {
        throw new DecodeException("epochs must be 0 for rejection " + code);
      }
      out = new MutateResult.Rejected(new FenceRejection(reason, carried, current));
    } else {
      throw new DecodeException("unknown fenced-mutate result " + kind);
    }
    r.end();
    return out;
  }

  // ── decisions ────────────────────────────────────────────────────────────

  public static byte[] encodeDecision(boolean allowed) {
    return new W(TAG_DECISION).bool(allowed).done();
  }

  public static boolean decodeDecision(byte[] buf) {
    R r = new R(buf, TAG_DECISION);
    boolean v = r.bool();
    r.end();
    return v;
  }

  // ── topology ─────────────────────────────────────────────────────────────

  public static byte[] encodeTopologyAdmittedRequest(List<String> nodeIds) {
    return new W(TAG_TOPOLOGY_ADMITTED_REQUEST).strs(nodeIds).done();
  }

  public static List<String> decodeTopologyAdmittedRequest(byte[] buf) {
    R r = new R(buf, TAG_TOPOLOGY_ADMITTED_REQUEST);
    List<String> v = r.strs();
    r.end();
    return v;
  }

  public static byte[] encodeTopologyAdmittedResponse(List<Boolean> admitted) {
    W w = new W(TAG_TOPOLOGY_ADMITTED_RESPONSE).list(admitted.size());
    for (Boolean b : admitted) {
      w.bool(b);
    }
    return w.done();
  }

  public static List<Boolean> decodeTopologyAdmittedResponse(byte[] buf) {
    R r = new R(buf, TAG_TOPOLOGY_ADMITTED_RESPONSE);
    int n = r.list();
    List<Boolean> v = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      v.add(r.bool());
    }
    r.end();
    return List.copyOf(v);
  }

  public static byte[] encodeTopologySnapshotRequest() {
    return new W(TAG_TOPOLOGY_SNAPSHOT_REQUEST).done();
  }

  public static void decodeTopologySnapshotRequest(byte[] buf) {
    new R(buf, TAG_TOPOLOGY_SNAPSHOT_REQUEST).end();
  }

  public static byte[] encodeTopologySnapshot(TopologySnapshot v) {
    W w = new W(TAG_TOPOLOGY_SNAPSHOT).list(v.clusters().size());
    for (ClusterView c : v.clusters()) {
      w.strs(c.members()).str(c.bridge()).strs(c.witnesses());
    }
    w.list(v.farPairs().size());
    for (FarPair p : v.farPairs()) {
      w.str(p.a()).str(p.b());
    }
    return w.uint(v.separationCoverage()).done();
  }

  public static TopologySnapshot decodeTopologySnapshot(byte[] buf) {
    R r = new R(buf, TAG_TOPOLOGY_SNAPSHOT);
    int n = r.list();
    List<ClusterView> clusters = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      clusters.add(new ClusterView(r.strs(), r.str(), r.strs()));
    }
    int m = r.list();
    List<FarPair> pairs = new ArrayList<>(m);
    for (int i = 0; i < m; i++) {
      pairs.add(new FarPair(r.str(), r.str()));
    }
    long coverage = r.uint();
    r.end();
    return new TopologySnapshot(List.copyOf(clusters), List.copyOf(pairs), coverage);
  }

  // ── expose / admission ───────────────────────────────────────────────────

  public static byte[] encodeExposeAuthorizeRequest(ExposeAuthorizeRequest v) {
    return new W(TAG_EXPOSE_AUTHORIZE_REQUEST)
        .str(v.serviceId())
        .str(v.peerId())
        .bytes(v.metadata())
        .done();
  }

  public static ExposeAuthorizeRequest decodeExposeAuthorizeRequest(byte[] buf) {
    R r = new R(buf, TAG_EXPOSE_AUTHORIZE_REQUEST);
    ExposeAuthorizeRequest v = new ExposeAuthorizeRequest(r.str(), r.str(), r.bytes());
    r.end();
    return v;
  }

  public static byte[] encodeAdmissionConnectRequest(AdmissionConnectRequest v) {
    return new W(TAG_ADMISSION_CONNECT_REQUEST).str(v.peer()).bytes(v.alpn()).done();
  }

  public static AdmissionConnectRequest decodeAdmissionConnectRequest(byte[] buf) {
    R r = new R(buf, TAG_ADMISSION_CONNECT_REQUEST);
    AdmissionConnectRequest v = new AdmissionConnectRequest(r.str(), r.bytes());
    r.end();
    return v;
  }

  public static byte[] encodeAdmissionHandshakeRequest(AdmissionHandshakeRequest v) {
    return new W(TAG_ADMISSION_HANDSHAKE_REQUEST)
        .str(v.peer())
        .bytes(v.alpn())
        .bool(v.isAlive())
        .done();
  }

  public static AdmissionHandshakeRequest decodeAdmissionHandshakeRequest(byte[] buf) {
    R r = new R(buf, TAG_ADMISSION_HANDSHAKE_REQUEST);
    AdmissionHandshakeRequest v = new AdmissionHandshakeRequest(r.str(), r.bytes(), r.bool());
    r.end();
    return v;
  }

  public static byte[] encodeAdmissionHandshakeResponse(HandshakeDecision d) {
    if (d.accept()) {
      return new W(TAG_ADMISSION_HANDSHAKE_RESPONSE).bool(true).uint(0).bytes(new byte[0]).done();
    }
    return new W(TAG_ADMISSION_HANDSHAKE_RESPONSE)
        .bool(false)
        .uint(d.code())
        .bytes(d.reason())
        .done();
  }

  public static HandshakeDecision decodeAdmissionHandshakeResponse(byte[] buf) {
    R r = new R(buf, TAG_ADMISSION_HANDSHAKE_RESPONSE);
    boolean accept = r.bool();
    long code = r.uintMax(0xFFFF_FFFFL, "reject code");
    byte[] reason = r.bytes();
    r.end();
    if (accept && (code != 0 || reason.length != 0)) {
      throw new DecodeException("accept carries no code or reason");
    }
    return new HandshakeDecision(accept, code, reason);
  }

  // ── expose HTTP / route policy (R9) ──────────────────────────────────────

  private static void putHeaders(W w, List<HttpHeader> headers) {
    w.list(headers.size());
    for (HttpHeader h : headers) {
      w.str(h.name()).bytes(h.value());
    }
  }

  private static List<HttpHeader> getHeaders(R r) {
    int n = r.list();
    List<HttpHeader> out = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      String name = r.str();
      if (name.isEmpty() || name.chars().anyMatch(c -> c >= 'A' && c <= 'Z')) {
        throw new DecodeException("header name \"" + name + "\" must be non-empty and lowercase");
      }
      out.add(new HttpHeader(name, r.bytes()));
    }
    return List.copyOf(out);
  }

  private static void putBody(W w, HttpBody body) {
    switch (body) {
      case HttpBody.Inline i -> w.u8(0).bytes(i.body());
      case HttpBody.Stream x -> w.u8(1);
    }
  }

  private static HttpBody getBody(R r) {
    int mode = r.u8();
    return switch (mode) {
      case 0 -> new HttpBody.Inline(r.bytes());
      case 1 -> HttpBody.STREAM;
      default -> throw new DecodeException("unknown body mode " + mode);
    };
  }

  public static byte[] encodeExposeHttpRequest(ExposeHttpRequest v) {
    W w = new W(TAG_EXPOSE_HTTP_REQUEST).str(v.serviceId());
    if (v.peerId() == null) {
      w.absent();
    } else {
      w.present().str(v.peerId());
    }
    w.uint(v.streamId()).u8(v.version()).str(v.method()).str(v.uri());
    putHeaders(w, v.headers());
    putBody(w, v.body());
    return w.done();
  }

  public static ExposeHttpRequest decodeExposeHttpRequest(byte[] buf) {
    R r = new R(buf, TAG_EXPOSE_HTTP_REQUEST);
    String serviceId = r.str();
    String peerId = r.present() ? r.str() : null;
    long streamId = r.uint();
    int version = r.u8();
    if (version > HTTP_VERSION_MAX) {
      throw new DecodeException("unknown HTTP version " + version);
    }
    String method = r.str();
    if (method.isEmpty()) {
      throw new DecodeException("empty HTTP method");
    }
    String uri = r.str();
    List<HttpHeader> headers = getHeaders(r);
    HttpBody body = getBody(r);
    r.end();
    return new ExposeHttpRequest(serviceId, peerId, streamId, version, method, uri, headers, body);
  }

  public static byte[] encodeExposeHttpResponse(ExposeHttpResponse v) {
    W w = new W(TAG_EXPOSE_HTTP_RESPONSE).uint(v.status());
    putHeaders(w, v.headers());
    putBody(w, v.body());
    return w.done();
  }

  public static ExposeHttpResponse decodeExposeHttpResponse(byte[] buf) {
    R r = new R(buf, TAG_EXPOSE_HTTP_RESPONSE);
    int status = (int) r.uintMax(999, "HTTP status");
    if (status < 100) {
      throw new DecodeException("HTTP status " + status + " below 100");
    }
    List<HttpHeader> headers = getHeaders(r);
    HttpBody body = getBody(r);
    r.end();
    return new ExposeHttpResponse(status, headers, body);
  }

  public static byte[] encodeExposeHttpTrailers(List<HttpHeader> trailers) {
    W w = new W(TAG_EXPOSE_HTTP_TRAILERS);
    putHeaders(w, trailers);
    return w.done();
  }

  public static List<HttpHeader> decodeExposeHttpTrailers(byte[] buf) {
    R r = new R(buf, TAG_EXPOSE_HTTP_TRAILERS);
    List<HttpHeader> v = getHeaders(r);
    r.end();
    return v;
  }

  public static byte[] encodeExposeRoutePolicyRequest(ExposeRoutePolicyRequest v) {
    W w = new W(TAG_EXPOSE_ROUTE_POLICY_REQUEST).str(v.peerId()).bytes(v.metadata());
    w.list(v.specs().size());
    for (ExposeRouteSpec s : v.specs()) {
      w.str(s.host()).uint(s.port()).u8(s.protocol()).str(s.serviceId());
    }
    return w.done();
  }

  public static ExposeRoutePolicyRequest decodeExposeRoutePolicyRequest(byte[] buf) {
    R r = new R(buf, TAG_EXPOSE_ROUTE_POLICY_REQUEST);
    String peerId = r.str();
    byte[] metadata = r.bytes();
    int n = r.list();
    List<ExposeRouteSpec> specs = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      String host = r.str();
      int port = (int) r.uintMax(0xFFFF, "port");
      int protocol = r.u8();
      if (protocol > ROUTE_PROTOCOL_TCP) {
        throw new DecodeException("unknown route protocol " + protocol);
      }
      specs.add(new ExposeRouteSpec(host, port, protocol, r.str()));
    }
    r.end();
    return new ExposeRoutePolicyRequest(peerId, metadata, List.copyOf(specs));
  }

  /** The granted subset of a route request: indexes into its specs, strictly ascending. */
  public static byte[] encodeExposeRouteGrant(List<Long> granted) {
    W w = new W(TAG_EXPOSE_ROUTE_GRANT).list(granted.size());
    for (Long i : granted) {
      w.uint(i);
    }
    return w.done();
  }

  public static List<Long> decodeExposeRouteGrant(byte[] buf) {
    R r = new R(buf, TAG_EXPOSE_ROUTE_GRANT);
    int n = r.list();
    List<Long> v = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      long idx = r.uintMax(0xFFFF_FFFFL, "route index");
      if (!v.isEmpty() && v.get(v.size() - 1) >= idx) {
        throw new DecodeException("granted route indexes not strictly ascending");
      }
      v.add(idx);
    }
    r.end();
    return List.copyOf(v);
  }
}
