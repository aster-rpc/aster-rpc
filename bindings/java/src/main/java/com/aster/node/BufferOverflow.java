package com.aster.node;

/** Buffer overflow strategy for SharedFlow. */
public enum BufferOverflow {
  /** Suspend the emitter until space becomes available. */
  SUSPEND,
  /** Drop the oldest element if buffer is full. */
  DROP_OLDEST,
  /** Drop the newest element if buffer is full. */
  DROP_NEWEST
}
