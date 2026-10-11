package site.aster.ffi;

/** Event kinds emitted by {@code aster_poll_events}. Values match Rust aster_event_kind_t enum. */
public enum IrohEventKind {
  NONE(0),

  // Lifecycle
  NODE_CREATED(1),
  NODE_CREATE_FAILED(2),
  ENDPOINT_CREATED(3),
  ENDPOINT_CREATE_FAILED(4),
  CLOSED(5),

  // Connections
  CONNECTED(10),
  CONNECT_FAILED(11),
  CONNECTION_ACCEPTED(12),
  CONNECTION_CLOSED(13),

  // Streams
  STREAM_OPENED(20),
  STREAM_ACCEPTED(21),
  FRAME_RECEIVED(22),
  SEND_COMPLETED(23),
  STREAM_FINISHED(24),
  STREAM_RESET(25),

  // Blobs
  BLOB_ADDED(30),
  BLOB_READ(31),
  BLOB_DOWNLOADED(32),
  BLOB_TICKET_CREATED(33),
  BLOB_COLLECTION_ADDED(34),
  BLOB_COLLECTION_TICKET_CREATED(35),

  // Tags
  TAG_SET(36),
  TAG_GET(37),
  TAG_DELETED(38),
  TAG_LIST(39),

  // Docs
  DOC_CREATED(40),
  DOC_JOINED(41),
  DOC_SET(42),
  DOC_GET(43),
  DOC_SHARED(44),
  AUTHOR_CREATED(45),
  DOC_QUERY(46),
  DOC_SUBSCRIBED(47),
  DOC_EVENT(48),
  DOC_JOINED_AND_SUBSCRIBED(49),

  // Gossip
  GOSSIP_SUBSCRIBED(50),
  GOSSIP_BROADCAST_DONE(51),
  GOSSIP_RECEIVED(52),
  GOSSIP_NEIGHBOR_UP(53),
  GOSSIP_NEIGHBOR_DOWN(54),
  GOSSIP_LAGGED(55),
  // Late additions in the 50s range (see aster_event_kind_t in ffi/aster_ffi.h)
  BLOB_OBSERVE_COMPLETE(56),
  DOC_DEL(57),

  // Datagrams
  // TODO(iroh): DATAGRAM_RECEIVED (60) is never emitted by any FFI function.
  // aster_connection_read_datagram emits BYTES_RESULT (91) instead.
  // Consider removing DATAGRAM_RECEIVED from this enum or clarifying its intended use.
  DATAGRAM_RECEIVED(60),

  // Aster custom-ALPN
  ASTER_ACCEPTED(65),

  // Hooks
  HOOK_BEFORE_CONNECT(70),
  HOOK_AFTER_CONNECT(71),
  HOOK_INVOCATION_RELEASED(72),

  // Registry async ops (§11.9)
  REGISTRY_RESOLVED(80),
  REGISTRY_PUBLISHED(81),
  REGISTRY_RENEWED(82),
  REGISTRY_ACL_UPDATED(83),
  REGISTRY_ACL_LISTED(84),

  // Generic
  STRING_RESULT(90),
  BYTES_RESULT(91),
  UNIT_RESULT(92),

  OPERATION_CANCELLED(98),
  ERROR(99),

  // Data plane (ABI 3.3)
  BLOB_FETCHED(100),
  DOC_AUTHORITY_REPORT(101),
  DOC_RETIRE_OUTCOME(102),
  DOC_OPENED(103),
  DOC_ENTRY_EXISTS(104),
  DOC_CONTENT_STATUS(105),
  STREAM_PRIORITY(106),

  // Upcall-backed leases (ABI 4.1)
  LEASE_ACQUIRED(107),
  LEASE_STATUS(108),

  // Streaming consumer relay readiness (ABI 4.3)
  HTTP_RELAY_READY(109),

  // Coordination client (ABI 4.4)
  COORDINATION_CLIENT_OPENED(110),
  COORDINATION_RESOLVED(111),
  COORDINATION_RESULT(112),
  COORDINATION_LEASE_ACQUIRED(113),

  // Durable audit outbox (ABI 4.9)
  AUDIT_OUTBOX_OPENED(114),
  AUDIT_OUTBOX_SUBMITTED(115),
  AUDIT_OUTBOX_STATUS(116),
  AUDIT_OUTBOX_EXPORTED(117),
  BLOB_CUSTODY_DONE(118),
  DOC_CONTENTS_BATCH(119),
  TOPOLOGY_SNAPSHOT(120);

  public final int code;

  IrohEventKind(int code) {
    this.code = code;
  }

  public static IrohEventKind fromCode(int code) {
    for (var v : values()) {
      if (v.code == code) return v;
    }
    return NONE;
  }
}
