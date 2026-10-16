package site.aster.identity;

import java.util.List;

/**
 * What the issuer-key publisher last did for one issuer ({@code
 * aster_enrollment_publisher_status}).
 *
 * @param fetchedAt Unix seconds of the last successful fetch, or {@code null}
 * @param lastError the last failure, or {@code null}
 */
public record IssuerPublishStatus(
    List<String> kids, Long fetchedAt, long publications, long attempts, String lastError) {

  public IssuerPublishStatus {
    kids = kids == null ? List.of() : List.copyOf(kids);
  }
}
