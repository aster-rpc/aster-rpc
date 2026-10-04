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
/// The binding-level settings at the end (trust, connect, logging) are read from the same
/// sources by this class.
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

    // ── Binding-level settings ────────────────────────────────────────────

    /// <summary>Open Gate 0 to every consumer (dev mode): the server's gate uses <see cref="TrustMode.OpenDev"/>.</summary>
    public bool AllowAllConsumers { get; init; }
    public bool AllowAllProducers { get; init; } = true;
    public byte[]? RootPubkey { get; init; }
    public string? RootPubkeyFile { get; init; }
    public string? EnrollmentCredentialFile { get; init; }
    public string? EnrollmentCredentialIid { get; init; }
    public string? EndpointAddr { get; init; }
    public string LogFormat { get; init; } = "text";
    public string LogLevel { get; init; } = "info";
    public bool LogMask { get; init; } = true;

    // ── Factories ─────────────────────────────────────────────────────────

    /// <summary>The Rust defaults (<c>aster_config_init</c>).</summary>
    public static unsafe AsterConfig Default()
    {
        ConfigNative n = default;
        Native.Check(Native.aster_config_init(&n), "aster_config_init");
        return FromNative(in n, new AsterConfig());
    }

    /// <summary>Rust <c>AsterConfig::from_env</c> plus the binding-level <c>ASTER_*</c> variables.</summary>
    public static unsafe AsterConfig FromEnv()
    {
        ConfigNative n = default;
        Native.Check(Native.aster_config_from_env(&n), "aster_config_from_env");
        try { return FromNative(in n, new AsterConfig()).WithBindingEnv(); }
        finally { Native.aster_config_release(&n); }
    }

    /// <summary>
    /// Rust <c>AsterConfig::from_file</c> (the TOML file, then <c>ASTER_*</c> overrides) plus the
    /// file's <c>[trust]</c> / <c>[connect]</c> / <c>[logging]</c> sections and their env overrides.
    /// </summary>
    public static unsafe AsterConfig FromToml(string path)
    {
        ConfigNative n = default;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_config_from_toml(scope.Utf8(path), &n), "aster_config_from_toml");
        AsterConfig c;
        try { c = FromNative(in n, new AsterConfig()); }
        finally { Native.aster_config_release(&n); }
        return c.WithBindingToml(path).WithBindingEnv();
    }

    /// <summary>Resolves the root public key from <see cref="RootPubkey"/> or <see cref="RootPubkeyFile"/>.</summary>
    public byte[]? ResolveRootPubkey()
    {
        if (RootPubkey != null) return RootPubkey;
        if (RootPubkeyFile != null && File.Exists(RootPubkeyFile))
        {
            var content = File.ReadAllText(RootPubkeyFile).Trim();
            if (content.Length == 64)
            {
                try { return Convert.FromHexString(content); } catch (FormatException) { }
            }
        }
        return null;
    }

    // ── aster_config_t marshalling ────────────────────────────────────────

    private const uint TBidi = 1, TUni = 1 << 1, TStreamRecv = 1 << 2, TRecv = 1 << 3, TSend = 1 << 4,
        TIdle = 1 << 5, TKeepAlive = 1 << 6, TMtu = 1 << 7, TDgramRecv = 1 << 8, TDgramSend = 1 << 9,
        TFairness = 1 << 10, TGso = 1 << 11;

    /// <summary>Builds an <c>aster_config_t</c> whose buffers live in <paramref name="scope"/>.</summary>
    internal unsafe ConfigNative ToNative(NativeScope scope, IEnumerable<string>? extraAlpns = null)
    {
        ConfigNative n = default;
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

        var trust = Sec("trust");
        c = c with
        {
            RootPubkeyFile = S(trust, "root_pubkey_file") ?? c.RootPubkeyFile,
            EnrollmentCredentialFile = S(trust, "enrollment_credential") ?? c.EnrollmentCredentialFile,
            EnrollmentCredentialIid = S(trust, "enrollment_credential_iid") ?? c.EnrollmentCredentialIid,
            AllowAllConsumers = B(trust, "allow_all_consumers") ?? c.AllowAllConsumers,
            AllowAllProducers = B(trust, "allow_all_producers") ?? c.AllowAllProducers,
            RootPubkey = S(trust, "root_pubkey") is { } hex ? Convert.FromHexString(hex) : c.RootPubkey,
            EndpointAddr = S(Sec("connect"), "endpoint_addr") ?? c.EndpointAddr,
        };
        var log = Sec("logging");
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
        byte[]? rootPubkey = RootPubkey;
        if (Env("ASTER_ROOT_PUBKEY") is { } hex)
        {
            try { rootPubkey = Convert.FromHexString(hex); } catch (FormatException) { }
        }
        return this with
        {
            RootPubkey = rootPubkey,
            RootPubkeyFile = Env("ASTER_ROOT_PUBKEY_FILE") ?? RootPubkeyFile,
            EnrollmentCredentialFile = Env("ASTER_ENROLLMENT_CREDENTIAL") ?? EnrollmentCredentialFile,
            EnrollmentCredentialIid = Env("ASTER_ENROLLMENT_CREDENTIAL_IID") ?? EnrollmentCredentialIid,
            EndpointAddr = Env("ASTER_ENDPOINT_ADDR") ?? EndpointAddr,
            AllowAllConsumers = EnvBool("ASTER_ALLOW_ALL_CONSUMERS") ?? AllowAllConsumers,
            AllowAllProducers = EnvBool("ASTER_ALLOW_ALL_PRODUCERS") ?? AllowAllProducers,
            LogFormat = Env("ASTER_LOG_FORMAT")?.ToLowerInvariant() ?? LogFormat,
            LogLevel = Env("ASTER_LOG_LEVEL")?.ToLowerInvariant() ?? LogLevel,
            LogMask = EnvBool("ASTER_LOG_MASK") ?? LogMask,
        };
    }

    /// <summary>Redacts the secret key.</summary>
    public override string ToString()
    {
        var sb = new StringBuilder("AsterConfig { ");
        sb.Append($"RelayMode = {RelayMode}, BindAddr = {BindAddr}, StoragePath = {StoragePath}, ");
        sb.Append($"Alpns = [{string.Join(", ", Alpns)}], SecretKey = {(SecretKey is null ? "null" : "<redacted>")}, ");
        sb.Append($"EnableHooks = {EnableHooks}, AllowAllConsumers = {AllowAllConsumers} }}");
        return sb.ToString();
    }
}
