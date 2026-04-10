package com.aster.ffi;

/** Event kinds emitted by {@code iroh_poll_events}. Values match Rust iroh_event_kind_t enum. */
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

  // Datagrams
  DATAGRAM_RECEIVED(60),

  // Aster custom-ALPN
  ASTER_ACCEPTED(65),

  // Hooks
  HOOK_BEFORE_CONNECT(70),
  HOOK_AFTER_CONNECT(71),
  HOOK_INVOCATION_RELEASED(72),

  // Generic
  STRING_RESULT(90),
  BYTES_RESULT(91),
  UNIT_RESULT(92),

  OPERATION_CANCELLED(98),
  ERROR(99);

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
