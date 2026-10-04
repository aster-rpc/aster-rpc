package site.aster.attestation;

import java.util.Arrays;
import java.util.List;
import java.util.Optional;

/**
 * The outcome of a successful {@link Attestation#verifyChain}. Identities are 32-byte Ed25519
 * public keys (node-level identities, not docs authors).
 *
 * @param node the node the chain binds (= the expected node)
 * @param anchor the trusted anchor the chain terminates at
 * @param intermediates delegating identities, node-ward to anchor-ward; empty for a single edge
 * @param depth number of edges
 */
public record VerifiedChain(byte[] node, byte[] anchor, List<byte[]> intermediates, int depth) {

  public VerifiedChain {
    node = node.clone();
    anchor = anchor.clone();
    intermediates = intermediates.stream().map(byte[]::clone).toList();
  }

  @Override
  public byte[] node() {
    return node.clone();
  }

  @Override
  public byte[] anchor() {
    return anchor.clone();
  }

  @Override
  public List<byte[]> intermediates() {
    return intermediates.stream().map(byte[]::clone).toList();
  }

  /** Whether {@code id} is the leaf node. */
  public boolean isNode(byte[] id) {
    return Arrays.equals(node, id);
  }

  /** Whether {@code id} is the trusted anchor. */
  public boolean isAnchor(byte[] id) {
    return Arrays.equals(anchor, id);
  }

  /** Whether {@code id} is an intermediate. */
  public boolean isIntermediate(byte[] id) {
    return intermediates.stream().anyMatch(m -> Arrays.equals(m, id));
  }

  /**
   * The role of {@code id} in this chain, or empty if it is not a participant ({@link
   * AttestationRole#NODE} wins if node and anchor coincide). Same rule as {@code
   * aster_verified_chain_role_of}.
   */
  public Optional<AttestationRole> roleOf(byte[] id) {
    if (isNode(id)) {
      return Optional.of(AttestationRole.NODE);
    }
    if (isAnchor(id)) {
      return Optional.of(AttestationRole.ANCHOR);
    }
    if (isIntermediate(id)) {
      return Optional.of(AttestationRole.INTERMEDIATE);
    }
    return Optional.empty();
  }
}
