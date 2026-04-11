package com.aster.node;

import com.aster.handle.IrohConnection;

/** The result of an accepted Aster connection. */
public record AcceptedAster(
    /** The ALPN protocol bytes negotiated for this connection. */
    byte[] alpn,
    /** The accepted connection. */
    IrohConnection connection) {}
