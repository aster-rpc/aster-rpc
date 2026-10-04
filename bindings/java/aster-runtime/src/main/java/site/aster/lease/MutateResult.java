package site.aster.lease;

import java.util.Arrays;

/** Result of a fenced mutation: applied (with output bytes) or rejected by the fence rule. */
public sealed interface MutateResult {

  /** The mutation ran; {@code output} is application-defined. */
  record Applied(byte[] output) implements MutateResult {
    @Override
    public boolean equals(Object o) {
      return o instanceof Applied a && Arrays.equals(output, a.output);
    }

    @Override
    public int hashCode() {
      return Arrays.hashCode(output);
    }

    @Override
    public String toString() {
      return "Applied[" + output.length + " bytes]";
    }
  }

  /** The fence rule refused the mutation. Surface it loudly; never retry silently. */
  record Rejected(FenceRejection rejection) implements MutateResult {}
}
