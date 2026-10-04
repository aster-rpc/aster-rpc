package site.aster.docs;

/**
 * The result of retiring local write authority ({@code aster::RetireOutcome}). {@link #busy()} is a
 * normal, fail-closed outcome: references to the document are still open.
 *
 * @param retired true if retired; false if refused as busy
 * @param access the access now stored (null when busy)
 * @param openReferences how many references remain (0 when retired)
 */
public record RetireOutcome(boolean retired, LocalAccess access, long openReferences) {

  public boolean busy() {
    return !retired;
  }
}
