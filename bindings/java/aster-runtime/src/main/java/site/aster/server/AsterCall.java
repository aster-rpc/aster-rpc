package site.aster.server;

/**
 * An incoming Aster RPC call delivered by the reactor.
 *
 * <p>The header and request payloads are already de-framed — they contain the raw payload bytes
 * without length prefix or flags byte. The flags are available via {@link #headerFlags()} and
 * {@link #requestFlags()}.
 *
 * @param callId reactor-assigned call ID (used internally for response correlation)
 * @param header de-framed header payload (contains StreamHeader: contract ID, method, etc.)
 * @param headerFlags the flags byte from the header frame
 * @param request de-framed request payload (contains the serialized RPC request)
 * @param requestFlags the flags byte from the request frame
 * @param peerId hex-encoded node ID of the remote peer
 * @param isSessionCall true if this call is part of a session (multi-frame) stream
 */
public record AsterCall(
    long callId,
    byte[] header,
    byte headerFlags,
    byte[] request,
    byte requestFlags,
    String peerId,
    boolean isSessionCall) {}
