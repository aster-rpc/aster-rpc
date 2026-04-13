package com.aster.registry;

/** All 6 normative gossip event types (Aster-SPEC.md §11.7). */
public enum GossipEventType {
  CONTRACT_PUBLISHED(0),
  CHANNEL_UPDATED(1),
  ENDPOINT_LEASE_UPSERTED(2),
  ENDPOINT_DOWN(3),
  ACL_CHANGED(4),
  COMPATIBILITY_PUBLISHED(5);

  private final int code;

  GossipEventType(int code) {
    this.code = code;
  }

  public int code() {
    return code;
  }

  public static GossipEventType fromCode(int code) {
    for (GossipEventType t : values()) {
      if (t.code == code) return t;
    }
    throw new IllegalArgumentException("Unknown GossipEventType code: " + code);
  }
}
