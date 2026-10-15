using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;
using Tomlyn;
using Tomlyn.Model;

namespace Aster;

/// <summary>NAT port mapping (<c>aster_portmapper_t</c>).</summary>
public enum Portmapper : uint
{
    /// <summary>Unset: the Rust default.</summary>
    Default = 0,
    Enabled = 1,
    Disabled = 2,
}

/// <summary>
/// QUIC transport tuning (<c>aster_config_t.transport_*</c>). <c>null</c> keeps the Rust default.
/// </summary>
public sealed record TransportConfig
{
    public ulong? MaxConcurrentBidiStreams { get; init; }
    public ulong? MaxConcurrentUniStreams { get; init; }
    public ulong? StreamReceiveWindow { get; init; }
    public ulong? ReceiveWindow { get; init; }
    public ulong? SendWindow { get; init; }
    public ulong? MaxIdleTimeoutMs { get; init; }
    public ulong? KeepAliveIntervalMs { get; init; }
    public uint? InitialMtu { get; init; }
    public ulong? DatagramReceiveBufferSize { get; init; }
    public ulong? DatagramSendBufferSize { get; init; }
    public bool? SendFairness { get; init; }
    public bool? EnableSegmentationOffload { get; init; }
}

/// <summary>
/// Node and server configuration. The node knobs map one-to-one onto the C ABI
/// <c>aster_config_t</c> (ABI 3.2) and are resolved by the Rust <c>aster::AsterConfig</c>, so
/// <see cref="FromEnv"/> / <see cref="FromToml"/> read <c>ASTER_*</c> variables and
/// <c>aster.toml</c> exactly as the Rust SDK and the other bindings do. Use <c>with</c>
/// expressions to override:
/// <code>
/// var config = AsterConfig.FromEnv() with { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
/// await using var node = await Node.StartAsync(config);
/// </code>
/// <c>[trust]</c> and <c>[enrollment]</c> are resolved by Rust too (C ABI 5.1); the binding-level
/// settings at the end (connect, logging, health) are read from the same sources by this class.
/// </summary>
public sealed record AsterConfig
{
    // ── Network (aster_config_t) ──────────────────────────────────────────

    /// <summary>"default", "disabled", "staging", "custom" (with <see cref="RelayUrls"/>), or a relay URL.</summary>
    public string? RelayMode { get; init; }
    public IReadOnlyList<string> RelayUrls { get; init; } = Array.Empty<string>();
    /// <summary>32-byte Ed25519 secret key; <c>null</c> = <see cref="SecretKeyFile"/> or ephemeral.</summary>
    public byte[]? SecretKey { get; init; }
    public string? SecretKeyFile { get; init; }
    /// <summary>Persistent store directory; <c>null</c> = in memory.</summary>
    public string? StoragePath { get; init; }
    /// <summary>Blob GC interval; 0 = the Rust default.</summary>
    public ulong GcIntervalMs { get; init; }
    /// <summary>e.g. "127.0.0.1:0"; <c>null</c> = all interfaces.</summary>
    public string? BindAddr { get; init; }
    /// <summary>ALPNs the node accepts, in addition to those added by the server/client.</summary>
    public IReadOnlyList<string> Alpns { get; init; } = Array.Empty<string>();
    public string? IdentityFile { get; init; }
    public string? ProxyUrl { get; init; }
    public bool ProxyFromEnv { get; init; }
    /// <summary>Hook (admission) timeout; 0 = 5000 ms.</summary>
    public ulong HookTimeoutMs { get; init; }
    public HookFailureMode HookFailureMode { get; init; } = HookFailureMode.FailOpen;
    /// <summary>Raise admission events (<see cref="Node.Admissions"/>) when no <see cref="Gate"/> is attached.</summary>
    public bool EnableHooks { get; init; }
    public bool EnableMonitoring { get; init; }
    public bool LocalDiscovery { get; init; }
    public bool ClearIpTransports { get; init; }
    public bool ClearRelayTransports { get; init; }
    public Portmapper Portmapper { get; init; }
    public TransportConfig Transport { get; init; } = new();
    /// <summary>
    /// zstd level for blob transport (Rust <c>AsterConfig::blob_zstd_level</c>): <c>null</c> = the
    /// default (3), 0 = off (the node serves and fetches blobs on the plain ALPN only), 1..19 = that
    /// level (larger values are clamped to 19). Env <c>ASTER_BLOB_ZSTD_LEVEL</c>; TOML
    /// <c>[storage] blob_zstd_level</c>.
    /// </summary>
    public int? BlobZstdLevel { get; init; }
    /// <summary>
    /// Re-bind a persistent node's last UDP ports on restart (Rust
    /// <c>AsterConfig::reuse_last_port</c>): <c>null</c> = the default (on). Env
    /// <c>ASTER_REUSE_LAST_PORT</c>; TOML <c>[network] reuse_last_port</c>.
    /// </summary>
    public bool? ReuseLastPort { get; init; }

    // ── Trust and Enrollment ([trust], [enrollment]; design §15.3) ────────
    //
    // Read by FromEnv / FromToml through the Rust aster::AsterConfig
    // (aster_config_enrollment_from_env / _from_toml), so names, defaults and precedence
    // (defaults < TOML < environment < code) are the Rust SDK's exactly. A node is open exactly
    // when enrollment is not configured (IsEnrollmentEnabled): there is no "allow all" switch.

    /// <summary>
    /// The trusted root (32 bytes): <see cref="RootPubkey"/>, or <see cref="RootPubkeyFile"/> (64
    /// hex, or JSON <c>{"public_key": "&lt;hex&gt;"}</c>); both set must agree. Env
    /// <c>ASTER_ROOT_PUBKEY(_FILE)</c>; TOML <c>[trust] root_pubkey(_file)</c>.
    /// </summary>
    public byte[]? RootPubkey { get; init; }
    public string? RootPubkeyFile { get; init; }
    /// <summary>
    /// Root rotation pin (64 hex); without it every rotation statement is refused. Env
    /// <c>ASTER_NEXT_ROOT_COMMITMENT</c>; TOML <c>[trust] next_root_commitment</c>.
    /// </summary>
    public string? NextRootCommitment { get; init; }
    /// <summary>Rotations applied to reach the root (<c>ASTER_ROOT_SEQ</c>, <c>[trust] root_seq</c>).</summary>
    public ulong RootSeq { get; init; }
    /// <summary>Applied rotation statements, oldest first (<c>ASTER_ROOT_ROTATIONS</c> comma-separated, <c>[trust] root_rotations</c>).</summary>
    public IReadOnlyList<string> RootRotations { get; init; } = Array.Empty<string>();
    /// <summary>
    /// This node's enrollment credential chain file (<c>aster enroll node --out</c>). Env
    /// <c>ASTER_ENROLLMENT_CREDENTIAL</c>; TOML <c>[trust] enrollment_credential</c>. Setting it
    /// enables enrollment.
    /// </summary>
    public string? EnrollmentCredentialFile { get; init; }
    /// <summary>
    /// The credential chain inline; wins over <see cref="EnrollmentCredentialFile"/>. A value that
    /// is not a chain is refused. Env <c>ASTER_ENROLLMENT_CREDENTIAL_CHAIN</c>; TOML
    /// <c>[trust] enrollment_credential_chain</c>.
    /// </summary>
    public string? EnrollmentCredentialChain { get; init; }
    /// <summary>The <c>[enrollment]</c> section (<c>ASTER_ENROLLMENT_*</c>).</summary>
    public EnrollmentConfig Enrollment { get; init; } = new();

    /// <summary>
    /// Whether <see cref="AsterServer"/> / <see cref="AsterClient"/> install Enrollment from this
    /// config: <see cref="EnrollmentConfig.Enabled"/> when set, else whether an audience, a fleet
    /// namespace, an issuer delegation or a credential (file or chain) is configured. The root and
    /// rotation pin alone do not enable it. (Rust <c>AsterConfig::is_enrollment_enabled</c>.)
    /// </summary>
    public bool IsEnrollmentEnabled => Enrollment.Enabled ?? (
        !string.IsNullOrWhiteSpace(Enrollment.Audience) || !string.IsNullOrWhiteSpace(Enrollment.FleetNamespace)
        || !string.IsNullOrWhiteSpace(Enrollment.IssuerDelegationFile) || !string.IsNullOrWhiteSpace(EnrollmentCredentialFile)
        || !string.IsNullOrWhiteSpace(EnrollmentCredentialChain));

    // ── Binding-level settings ([connect], [logging]) ─────────────────────

    public string? EndpointAddr { get; init; }
    public string LogFormat { get; init; } = "text";
    public string LogLevel { get; init; } = "info";
    public bool LogMask { get; init; } = true;

    // ── Ops endpoints (/healthz, /readyz, /metrics; Rust aster::ops) ──────

    /// <summary>TCP port of the ops listener (<c>ASTER_HEALTH_PORT</c>, <c>[health] port</c>). 0 = disabled unless <see cref="HealthEnabled"/>.</summary>
    public ushort HealthPort { get; init; }
    /// <summary>Bind host (<c>ASTER_HEALTH_HOST</c>, <c>[health] host</c>); localhost only by default, pod probes need <c>0.0.0.0</c>.</summary>
    public string HealthHost { get; init; } = "127.0.0.1";
    /// <summary>Forces the listener on or off (<c>ASTER_HEALTH_ENABLED</c>, <c>[health] enabled</c>); <c>null</c> enables it when <see cref="HealthPort"/> is not 0.</summary>
    public bool? HealthEnabled { get; init; }
    /// <summary>Whether the ops listener should run: <see cref="HealthEnabled"/> when set, else <c>HealthPort != 0</c>.</summary>
    public bool IsHealthEnabled => HealthEnabled ?? HealthPort != 0;

    // ── Factories ─────────────────────────────────────────────────────────

    /// <summary>The Rust defaults (<c>aster_config_init</c>).</summary>
    public static unsafe AsterConfig Default()
    {
        ConfigNative n = default;
        Native.EnsureAbi();
        Native.Check(Native.aster_config_init(&n), "aster_config_init");
        return FromNative(in n, new AsterConfig());
    }

    /// <summary>
    /// Rust <c>AsterConfig::from_env</c>: the node settings, <c>[trust]</c> and <c>[enrollment]</c>
    /// (<c>ASTER_ROOT_*</c>, <c>ASTER_ENROLLMENT_*</c>), plus the binding-level <c>ASTER_*</c>
    /// variables. A malformed variable throws <see cref="IrohException"/> (<c>InvalidArgument</c>).
    /// </summary>
    public static unsafe AsterConfig FromEnv()
    {
        ConfigNative n = default;
        Native.EnsureAbi();
        Native.Check(Native.aster_config_from_env(&n), "aster_config_from_env");
        AsterConfig c;
        try { c = FromNative(in n, new AsterConfig()).WithBindingEnv(); }
        finally { Native.aster_config_release(&n); }
        Bytes trust = default;
        Native.Check(Native.aster_config_enrollment_from_env(&trust), "aster_config_enrollment_from_env");
        return c.WithTrust(TrustJson.Parse(NativeScope.TakeOwned(ref trust)));
    }

    /// <summary>
    /// Rust <c>AsterConfig::from_file</c> (the TOML file, then <c>ASTER_*</c> overrides), including
    /// <c>[trust]</c> and <c>[enrollment]</c>, plus the file's <c>[connect]</c> / <c>[logging]</c> /
    /// <c>[health]</c> sections and their env overrides.
    /// </summary>
    public static unsafe AsterConfig FromToml(string path)
    {
        ConfigNative n = default;
        Bytes trust = default;
        Native.EnsureAbi();
        using (var scope = new NativeScope())
            Native.Check(Native.aster_config_from_toml(scope.Utf8(path), &n), "aster_config_from_toml");
        AsterConfig c;
        try { c = FromNative(in n, new AsterConfig()); }
        finally { Native.aster_config_release(&n); }
        using (var scope = new NativeScope())
            Native.Check(Native.aster_config_enrollment_from_toml(scope.Utf8(path), &trust), "aster_config_enrollment_from_toml");
        return c.WithBindingToml(path).WithBindingEnv().WithTrust(TrustJson.Parse(NativeScope.TakeOwned(ref trust)));
    }

    /// <summary>
    /// The configured root public key: <see cref="RootPubkey"/>, else the key in
    /// <see cref="RootPubkeyFile"/> (64 hex, or JSON <c>{"public_key": "&lt;hex&gt;"}</c>); null when
    /// neither yields a 32-byte key.
    /// </summary>
    public byte[]? ResolveRootPubkey()
    {
        if (RootPubkey is { Length: 32 }) return RootPubkey;
        if (RootPubkeyFile == null || !File.Exists(RootPubkeyFile)) return null;
        var content = File.ReadAllText(RootPubkeyFile).Trim();
        try
        {
            if (content.StartsWith('{'))
                content = System.Text.Json.JsonDocument.Parse(content).RootElement.GetProperty("public_key").GetString() ?? "";
            var key = Convert.FromHexString(content);
            return key.Length == 32 ? key : null;
        }
        catch (Exception e) when (e is FormatException or System.Text.Json.JsonException or KeyNotFoundException or InvalidOperationException)
        {
            return null;
        }
    }

    // ── [trust] + [enrollment] JSON (C ABI 5.1) ───────────────────────────

    private AsterConfig WithTrust(TrustJson t) => this with
    {
        RootPubkey = string.IsNullOrWhiteSpace(t.RootPubkey) ? null : Convert.FromHexString(t.RootPubkey.Trim()),
        RootPubkeyFile = t.RootPubkeyFile,
        NextRootCommitment = t.NextRootCommitment,
        RootSeq = t.RootSeq,
        RootRotations = t.RootRotations,
        EnrollmentCredentialFile = t.EnrollmentCredentialFile,
        EnrollmentCredentialChain = t.EnrollmentCredentialChain,
        Enrollment = t.Enrollment,
    };

    /// <summary>The trust JSON <c>aster_node_start_enrolled</c> takes.</summary>
    internal byte[] TrustJsonBytes()
    {
        if (RootPubkey is { } key && key.Length != 32)
            throw new ArgumentException($"RootPubkey: {key.Length} bytes, want 32", nameof(RootPubkey));
        return new TrustJson
        {
            RootPubkey = RootPubkey is null ? null : Convert.ToHexString(RootPubkey).ToLowerInvariant(),
            RootPubkeyFile = RootPubkeyFile,
            NextRootCommitment = NextRootCommitment,
            RootSeq = RootSeq,
            RootRotations = RootRotations.ToList(),
            EnrollmentCredentialFile = EnrollmentCredentialFile,
            EnrollmentCredentialChain = EnrollmentCredentialChain,
            Enrollment = Enrollment,
        }.Encode();
    }

    // ── aster_config_t marshalling ────────────────────────────────────────

    // ASTER_BLOB_ZSTD_LEVEL_* and aster_reuse_last_port_t (C ABI 6.0).
    private const int BlobZstdLevelDefault = 0, BlobZstdLevelOff = -1, BlobZstdLevelMax = 19;
    private const uint ReuseLastPortDefault = 0, ReuseLastPortEnabled = 1, ReuseLastPortDisabled = 2;

    private const uint TBidi = 1, TUni = 1 << 1, TStreamRecv = 1 << 2, TRecv = 1 << 3, TSend = 1 << 4,
        TIdle = 1 << 5, TKeepAlive = 1 << 6, TMtu = 1 << 7, TDgramRecv = 1 << 8, TDgramSend = 1 << 9,
        TFairness = 1 << 10, TGso = 1 << 11;

    /// <summary>Builds an <c>aster_config_t</c> whose buffers live in <paramref name="scope"/>.</summary>
    internal unsafe ConfigNative ToNative(NativeScope scope, IEnumerable<string>? extraAlpns = null)
    {
        ConfigNative n = default;
        Native.EnsureAbi();
        Native.Check(Native.aster_config_init(&n), "aster_config_init");

        var urls = RelayUrls.ToList();
        string mode = RelayMode?.Trim().ToLowerInvariant() ?? string.Empty;
        n.relay_mode = mode switch
        {
            "" or "default" => urls.Count == 0 ? (uint)Aster.RelayMode.Default : (uint)Aster.RelayMode.Custom,
            "custom" => (uint)Aster.RelayMode.Custom,
            "disabled" => (uint)Aster.RelayMode.Disabled,
            "staging" => (uint)Aster.RelayMode.Staging,
            _ => (uint)Aster.RelayMode.Custom, // a bare URL means custom (same rule as Rust)
        };
        if (n.relay_mode == (uint)Aster.RelayMode.Custom && urls.Count == 0 && mode is not ("" or "default" or "custom"))
            urls.Add(RelayMode!.Trim());
        n.relay_urls = scope.Utf8List(urls);
        if (SecretKey is { Length: > 0 }) n.secret_key = scope.Bytes(SecretKey);
        n.secret_key_file = scope.Utf8(SecretKeyFile);
        n.storage_path = scope.Utf8(StoragePath);
        n.gc_interval_ms = GcIntervalMs;
        n.bind_addr = scope.Utf8(BindAddr);
        var alpns = Alpns.Concat(extraAlpns ?? Enumerable.Empty<string>()).Distinct().ToList();
        n.alpns = scope.Utf8List(alpns);
        n.identity_file = scope.Utf8(IdentityFile);
        n.proxy_url = scope.Utf8(ProxyUrl);
        n.hook_timeout_ms = HookTimeoutMs;
        n.hook_failure_mode = (uint)HookFailureMode;
        n.enable_hooks = EnableHooks ? 1u : 0u;
        n.enable_monitoring = EnableMonitoring ? 1u : 0u;
        n.local_discovery = LocalDiscovery ? 1u : 0u;
        n.clear_ip_transports = ClearIpTransports ? 1u : 0u;
        n.clear_relay_transports = ClearRelayTransports ? 1u : 0u;
        n.portmapper = (uint)Portmapper;
        n.proxy_from_env = ProxyFromEnv ? 1u : 0u;
        n.blob_zstd_level = BlobZstdLevel switch
        {
            null => BlobZstdLevelDefault,
            < 0 => throw new ArgumentException($"BlobZstdLevel must be 0 (off) or a level, got {BlobZstdLevel}", nameof(BlobZstdLevel)),
            0 => BlobZstdLevelOff,
            int level => Math.Min(level, BlobZstdLevelMax),
        };
        n.reuse_last_port = ReuseLastPort switch
        {
            null => ReuseLastPortDefault,
            true => ReuseLastPortEnabled,
            false => ReuseLastPortDisabled,
        };

        uint set = 0;
        var t = Transport;
        void U64(ulong? v, uint bit, ref ulong field) { if (v is { } x) { field = x; set |= bit; } }
        U64(t.MaxConcurrentBidiStreams, TBidi, ref n.transport_max_concurrent_bidi_streams);
        U64(t.MaxConcurrentUniStreams, TUni, ref n.transport_max_concurrent_uni_streams);
        U64(t.StreamReceiveWindow, TStreamRecv, ref n.transport_stream_receive_window);
        U64(t.ReceiveWindow, TRecv, ref n.transport_receive_window);
        U64(t.SendWindow, TSend, ref n.transport_send_window);
        U64(t.MaxIdleTimeoutMs, TIdle, ref n.transport_max_idle_timeout_ms);
        U64(t.KeepAliveIntervalMs, TKeepAlive, ref n.transport_keep_alive_interval_ms);
        U64(t.DatagramReceiveBufferSize, TDgramRecv, ref n.transport_datagram_receive_buffer_size);
        U64(t.DatagramSendBufferSize, TDgramSend, ref n.transport_datagram_send_buffer_size);
        if (t.InitialMtu is { } mtu) { n.transport_initial_mtu = mtu; set |= TMtu; }
        if (t.SendFairness is { } fair) { n.transport_send_fairness = fair ? 1u : 0u; set |= TFairness; }
        if (t.EnableSegmentationOffload is { } gso) { n.transport_enable_segmentation_offload = gso ? 1u : 0u; set |= TGso; }
        n.transport_set = set;
        return n;
    }

    private static AsterConfig FromNative(in ConfigNative n, AsterConfig b)
    {
        string? Str(Bytes x) => x.IsEmpty ? null : x.ToUtf8String();
        uint set = n.transport_set;
        ulong? U64(uint bit, ulong v) => (set & bit) != 0 ? v : null;
        return b with
        {
            RelayMode = (RelayMode)n.relay_mode switch
            {
                Aster.RelayMode.Custom => "custom",
                Aster.RelayMode.Disabled => "disabled",
                Aster.RelayMode.Staging => "staging",
                _ => "default",
            },
            RelayUrls = NativeScope.ReadUtf8List(n.relay_urls),
            SecretKey = n.secret_key.IsEmpty ? null : n.secret_key.ToArray(),
            SecretKeyFile = Str(n.secret_key_file),
            StoragePath = Str(n.storage_path),
            GcIntervalMs = n.gc_interval_ms,
            BindAddr = Str(n.bind_addr),
            Alpns = NativeScope.ReadUtf8List(n.alpns),
            IdentityFile = Str(n.identity_file),
            ProxyUrl = Str(n.proxy_url),
            HookTimeoutMs = n.hook_timeout_ms,
            HookFailureMode = (HookFailureMode)n.hook_failure_mode,
            EnableHooks = n.enable_hooks != 0,
            EnableMonitoring = n.enable_monitoring != 0,
            LocalDiscovery = n.local_discovery != 0,
            ClearIpTransports = n.clear_ip_transports != 0,
            ClearRelayTransports = n.clear_relay_transports != 0,
            Portmapper = (Portmapper)n.portmapper,
            ProxyFromEnv = n.proxy_from_env != 0,
            BlobZstdLevel = n.blob_zstd_level switch
            {
                BlobZstdLevelOff => 0,
                > 0 => n.blob_zstd_level,
                _ => null,
            },
            ReuseLastPort = n.reuse_last_port switch
            {
                ReuseLastPortEnabled => true,
                ReuseLastPortDisabled => false,
                _ => null,
            },
            Transport = new TransportConfig
            {
                MaxConcurrentBidiStreams = U64(TBidi, n.transport_max_concurrent_bidi_streams),
                MaxConcurrentUniStreams = U64(TUni, n.transport_max_concurrent_uni_streams),
                StreamReceiveWindow = U64(TStreamRecv, n.transport_stream_receive_window),
                ReceiveWindow = U64(TRecv, n.transport_receive_window),
                SendWindow = U64(TSend, n.transport_send_window),
                MaxIdleTimeoutMs = U64(TIdle, n.transport_max_idle_timeout_ms),
                KeepAliveIntervalMs = U64(TKeepAlive, n.transport_keep_alive_interval_ms),
                DatagramReceiveBufferSize = U64(TDgramRecv, n.transport_datagram_receive_buffer_size),
                DatagramSendBufferSize = U64(TDgramSend, n.transport_datagram_send_buffer_size),
                InitialMtu = (set & TMtu) != 0 ? n.transport_initial_mtu : null,
                SendFairness = (set & TFairness) != 0 ? n.transport_send_fairness != 0 : null,
                EnableSegmentationOffload = (set & TGso) != 0 ? n.transport_enable_segmentation_offload != 0 : null,
            },
        };
    }

    // ── binding-level sources ─────────────────────────────────────────────

    private AsterConfig WithBindingToml(string path)
    {
        var model = TomlSerializer.Deserialize<TomlTable>(File.ReadAllText(path))
            ?? throw new InvalidOperationException($"Failed to parse TOML from {path}");
        var c = this;
        TomlTable? Sec(string name) => model.TryGetValue(name, out var o) && o is TomlTable t ? t : null;
        static string? S(TomlTable? t, string k) => t != null && t.TryGetValue(k, out var v) && v is string s ? s : null;
        static bool? B(TomlTable? t, string k) => t != null && t.TryGetValue(k, out var v) && v is bool b ? b : null;

        // [trust] and [enrollment] come from Rust (aster_config_enrollment_from_toml).
        c = c with { EndpointAddr = S(Sec("connect"), "endpoint_addr") ?? c.EndpointAddr };
        var log = Sec("logging");
        var health = Sec("health");
        c = c with
        {
            HealthPort = health != null && health.TryGetValue("port", out var hp) && hp is long port
                ? ToPort(port, "[health] port") : c.HealthPort,
            HealthHost = S(health, "host") ?? c.HealthHost,
            HealthEnabled = B(health, "enabled") ?? c.HealthEnabled,
        };
        return c with
        {
            LogFormat = S(log, "format")?.ToLowerInvariant() ?? c.LogFormat,
            LogLevel = S(log, "level")?.ToLowerInvariant() ?? c.LogLevel,
            LogMask = B(log, "mask") ?? c.LogMask,
        };
    }

    private AsterConfig WithBindingEnv()
    {
        static string? Env(string k) => Environment.GetEnvironmentVariable(k)?.Trim() is { Length: > 0 } v ? v : null;
        static bool? EnvBool(string k) => Env(k)?.ToLowerInvariant() switch
        {
            "true" or "1" or "yes" or "on" => true,
            "false" or "0" or "no" or "off" => false,
            _ => null,
        };
        // ASTER_ROOT_* and ASTER_ENROLLMENT_* come from Rust (aster_config_enrollment_from_*).
        return this with
        {
            EndpointAddr = Env("ASTER_ENDPOINT_ADDR") ?? EndpointAddr,
            LogFormat = Env("ASTER_LOG_FORMAT")?.ToLowerInvariant() ?? LogFormat,
            LogLevel = Env("ASTER_LOG_LEVEL")?.ToLowerInvariant() ?? LogLevel,
            LogMask = EnvBool("ASTER_LOG_MASK") ?? LogMask,
            HealthPort = Env("ASTER_HEALTH_PORT") is { } hp
                ? (long.TryParse(hp, out var port) ? ToPort(port, "ASTER_HEALTH_PORT")
                    : throw new FormatException($"ASTER_HEALTH_PORT: expected a port number, got \"{hp}\""))
                : HealthPort,
            HealthHost = Env("ASTER_HEALTH_HOST") ?? HealthHost,
            HealthEnabled = EnvBool("ASTER_HEALTH_ENABLED") ?? HealthEnabled,
        };
    }

    private static ushort ToPort(long v, string what)
        => v is >= 0 and <= ushort.MaxValue ? (ushort)v : throw new FormatException($"{what}: out of range: {v}");

    /// <summary>Redacts the secret key.</summary>
    public override string ToString()
    {
        var sb = new StringBuilder("AsterConfig { ");
        sb.Append($"RelayMode = {RelayMode}, BindAddr = {BindAddr}, StoragePath = {StoragePath}, ");
        sb.Append($"Alpns = [{string.Join(", ", Alpns)}], SecretKey = {(SecretKey is null ? "null" : "<redacted>")}, ");
        sb.Append($"EnableHooks = {EnableHooks}, Enrollment = {IsEnrollmentEnabled} }}");
        return sb.ToString();
    }
}
