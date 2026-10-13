package site.aster.enrollment;

import java.util.List;

/**
 * The {@code [enrollment]} section of {@link site.aster.config.AsterConfig} (Rust {@code
 * aster::EnrollmentConfig}; env {@code ASTER_ENROLLMENT_*}; design §15.3). The root and this node's
 * credential live in the config's trust fields. {@code AsterConfig.fromEnv()} / {@code fromFile()}
 * fill it through the Rust config loader, so names, defaults and precedence are the Rust SDK's.
 * Once enabled every value is validated when the server or client starts and any error stops the
 * start: enrollment never degrades to an open node.
 */
public final class EnrollmentConfig {

  /** Nothing configured (enrollment off unless the credential is set). */
  public static final EnrollmentConfig DEFAULTS = builder().build();

  private Boolean enabled;
  private String audience;
  private String fleetNamespace;
  private String statePath;
  private List<String> replicas = List.of();
  private Policy policy;
  private boolean requireSynchronizedClock;
  private Long activationLeewaySecs;
  private EnrollmentLimits limits = EnrollmentLimits.DEFAULTS;
  private RoleMapping roles = RoleMapping.DEFAULTS;
  private String issuerDelegationFile;
  private Long issuerLifetimeSecs;
  private String renewFrom;
  private Boolean renew;
  private Boolean synchronize;

  private EnrollmentConfig() {}

  /**
   * Forces enrollment on or off; {@code null} enables it exactly when an audience, fleet namespace,
   * issuer delegation or credential is configured. TOML {@code enabled}; env {@code
   * ASTER_ENROLLMENT_ENABLED}.
   */
  public Boolean enabled() {
    return enabled;
  }

  /** The fleet audience credentials must name. Required. {@code ASTER_ENROLLMENT_AUDIENCE}. */
  public String audience() {
    return audience;
  }

  /**
   * The fleet policy namespace (64 hex; {@code aster enroll namespace create}). Required. {@code
   * ASTER_ENROLLMENT_FLEET_NAMESPACE}.
   */
  public String fleetNamespace() {
    return fleetNamespace;
  }

  /**
   * Durable denials and watermarks. Default {@code <storage path>/enrollment.json}; a server with
   * neither refuses to start, a client uses a temporary directory. {@code
   * ASTER_ENROLLMENT_STATE_PATH}.
   */
  public String statePath() {
    return statePath;
  }

  /**
   * Policy replicas ({@code aster1…} tickets or 64-hex node ids) a server enrolls with and
   * synchronizes the fleet namespace from at start. Empty: the server holds the fleet namespace and
   * the root publishes into it. {@code ASTER_ENROLLMENT_REPLICAS} (comma-separated).
   */
  public List<String> replicas() {
    return replicas;
  }

  /**
   * The admission policy; {@code null} = {@link Policy#tolerateStale()}. TOML {@code policy}
   * (inline table or string); env {@code ASTER_ENROLLMENT_POLICY} (JSON).
   */
  public Policy policy() {
    return policy;
  }

  /** Refuse to start on an unsynchronized clock (Linux). */
  public boolean requireSynchronizedClock() {
    return requireSynchronizedClock;
  }

  /** {@code iat}/{@code nbf} leeway in seconds, 0–300; {@code null} = 60. */
  public Long activationLeewaySecs() {
    return activationLeewaySecs;
  }

  /** {@code [enrollment.limits]}. */
  public EnrollmentLimits limits() {
    return limits;
  }

  /** {@code [enrollment.roles]}: how verified claims become Gate-3 roles. */
  public RoleMapping roles() {
    return roles;
  }

  /**
   * Server only: run a delegated issuer and serve {@code aster.enrollment.IssuerV1} ({@code aster
   * enroll delegate --out}). A client with it set is refused. {@code [enrollment.issuer]
   * delegation_file}; {@code ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE}.
   */
  public String issuerDelegationFile() {
    return issuerDelegationFile;
  }

  /**
   * Lifetime of issued credentials in seconds, shortened to the delegation; {@code null} = 24 h.
   * {@code [enrollment.issuer] lifetime}; {@code ASTER_ENROLLMENT_ISSUER_LIFETIME}.
   */
  public Long issuerLifetimeSecs() {
    return issuerLifetimeSecs;
  }

  /** The issuer to renew this node's credential from (ticket or node id). */
  public String renewFrom() {
    return renewFrom;
  }

  /**
   * {@code false} disables renewal; {@code null} renews from {@link #renewFrom()}, or a client from
   * the server that signed its credential.
   */
  public Boolean renew() {
    return renew;
  }

  /**
   * Client: synchronize the fleet namespace the server offers ({@code null} = true), so the client
   * learns revocations of the server too.
   */
  public Boolean synchronize() {
    return synchronize;
  }

  public Builder toBuilder() {
    Builder b = new Builder();
    b.c.enabled = enabled;
    b.c.audience = audience;
    b.c.fleetNamespace = fleetNamespace;
    b.c.statePath = statePath;
    b.c.replicas = replicas;
    b.c.policy = policy;
    b.c.requireSynchronizedClock = requireSynchronizedClock;
    b.c.activationLeewaySecs = activationLeewaySecs;
    b.c.limits = limits;
    b.c.roles = roles;
    b.c.issuerDelegationFile = issuerDelegationFile;
    b.c.issuerLifetimeSecs = issuerLifetimeSecs;
    b.c.renewFrom = renewFrom;
    b.c.renew = renew;
    b.c.synchronize = synchronize;
    return b;
  }

  public static Builder builder() {
    return new Builder();
  }

  /** Builder; every setter mirrors the getter of the same name. */
  public static final class Builder {
    private final EnrollmentConfig c = new EnrollmentConfig();

    private Builder() {}

    public Builder enabled(Boolean v) {
      c.enabled = v;
      return this;
    }

    public Builder audience(String v) {
      c.audience = v;
      return this;
    }

    public Builder fleetNamespace(String v) {
      c.fleetNamespace = v;
      return this;
    }

    public Builder statePath(String v) {
      c.statePath = v;
      return this;
    }

    public Builder replicas(List<String> v) {
      c.replicas = v == null ? List.of() : List.copyOf(v);
      return this;
    }

    public Builder policy(Policy v) {
      c.policy = v;
      return this;
    }

    public Builder requireSynchronizedClock(boolean v) {
      c.requireSynchronizedClock = v;
      return this;
    }

    public Builder activationLeewaySecs(Long v) {
      c.activationLeewaySecs = v;
      return this;
    }

    public Builder limits(EnrollmentLimits v) {
      c.limits = v == null ? EnrollmentLimits.DEFAULTS : v;
      return this;
    }

    public Builder roles(RoleMapping v) {
      c.roles = v == null ? RoleMapping.DEFAULTS : v;
      return this;
    }

    public Builder issuerDelegationFile(String v) {
      c.issuerDelegationFile = v;
      return this;
    }

    public Builder issuerLifetimeSecs(Long v) {
      c.issuerLifetimeSecs = v;
      return this;
    }

    public Builder renewFrom(String v) {
      c.renewFrom = v;
      return this;
    }

    public Builder renew(Boolean v) {
      c.renew = v;
      return this;
    }

    public Builder synchronize(Boolean v) {
      c.synchronize = v;
      return this;
    }

    /** A new config (the builder may be reused). */
    public EnrollmentConfig build() {
      return c.toBuilder().c;
    }
  }
}
