package site.aster.server;

/**
 * An incoming Aster RPC call delivered by the reactor.
 *
 * <p>The header and request payloads are already de-framed — they contain the raw payload bytes
 * without length prefix or flags byte. The flags are available via {@link #headerFlags()} and
 * {@link #requestFlags()}.
 *
 * @param callId reactor-assigned call ID (used internally for response correlation)
 * @param connectionId reactor-assigned id for the QUIC connection this call arrived on. Sessions
 *     are scoped per-{@code (peer, connection)} (spec §7.5); use this together with the {@code
 *     sessionId} decoded from the {@code StreamHeader} payload to key per-session state.
 * @param streamId reactor-assigned unique ID for the QUIC bi-stream. With multiplexed streams a
 *     single bi-stream may carry many calls, so DO NOT key per-session state on this — use {@code
 *     (connectionId, sessionId)} instead.
 * @param header de-framed header payload (contains StreamHeader: contract ID, method, sessionId,
 *     etc.)
 * @param headerFlags the flags byte from the header frame
 * @param request de-framed request payload (contains the serialized RPC request)
 * @param requestFlags the flags byte from the request frame
 * @param peerId hex-encoded node ID of the remote peer
 */
public record AsterCall(
    long callId,
    long connectionId,
    long streamId,
    byte[] header,
    byte headerFlags,
    byte[] request,
    byte requestFlags,
    String peerId) {}
