package site.aster.interceptors;

import java.util.Map;

/**
 * Raised when a wire payload doesn't match the published contract.
 *
 * <p>The producer owns the contract: consumers must use the field names defined by the producer's
 * manifest. Sending an extra key — at any nesting depth — is a contract violation, not a tolerable
 * mismatch. {@link JsonCodec}-style strict decoders raise this before the handler runs, so the
 * error surfaces as a {@link StatusCode#CONTRACT_VIOLATION} trailer on the wire.
 *
 * <p>Carries the offending field names in {@code details} under {@code unexpected_fields}
 * (comma-separated), the dotted violation path under {@code location}, and the expected dataclass
 * name under {@code expected_class}.
 */
public class ContractViolationError extends RpcError {

  public ContractViolationError(String message) {
    super(StatusCode.CONTRACT_VIOLATION, message);
  }

  public ContractViolationError(String message, Map<String, String> details) {
    super(StatusCode.CONTRACT_VIOLATION, message, details);
  }
}
