using System;
using System.IO;
using System.Text;

namespace Aster;

/// <summary>
/// Unified configuration for AsterServer.
///
/// Three-layer resolution (later wins):
/// 1. Built-in defaults (ephemeral key, in-memory store, all gates open).
/// 2. TOML config file (aster.toml) -- requires Tomlyn NuGet (future).
/// 3. ASTER_* environment variables.
///
/// Usage:
///   var config = AsterConfig.FromEnv();
///   // or inline:
///   var config = new AsterConfig { AllowAllConsumers = true, StoragePath = "/var/lib/aster" };
/// </summary>
public class AsterConfig
{
    // ── Trust ────────────────────────────────────────────────────────────
    public byte[]? RootPubkey { get; set; }
    public string? RootPubkeyFile { get; set; }
    public string? EnrollmentCredentialFile { get; set; }
    public string? EnrollmentCredentialIid { get; set; }
    public bool AllowAllConsumers { get; set; }
    public bool AllowAllProducers { get; set; } = true;

    // ── Connect ──────────────────────────────────────────────────────────
    public string? EndpointAddr { get; set; }

    // ── Storage ──────────────────────────────────────────────────────────
    public string? StoragePath { get; set; }

    // ── Network ──────────────────────────────────────────────────────────
    public byte[]? SecretKey { get; set; }
    public string? RelayMode { get; set; }
    public string? BindAddr { get; set; }
    public bool EnableMonitoring { get; set; }
    public bool EnableHooks { get; set; }
    public int HookTimeoutMs { get; set; } = 5000;
    public bool ClearIpTransports { get; set; }
    public bool ClearRelayTransports { get; set; }
    public string? PortmapperConfig { get; set; }
    public string? ProxyUrl { get; set; }
    public bool ProxyFromEnv { get; set; }
    public bool LocalDiscovery { get; set; }

    // ── Logging ──────────────────────────────────────────────────────────
    public string LogFormat { get; set; } = "text";
    public string LogLevel { get; set; } = "info";
    public bool LogMask { get; set; } = true;

    // ── Identity ─────────────────────────────────────────────────────────
    public string? IdentityFile { get; set; }

    // ── Factory ──────────────────────────────────────────────────────────

    /// <summary>Build config from ASTER_* environment variables only.</summary>
    public static AsterConfig FromEnv()
    {
        var c = new AsterConfig();
        c.ApplyEnv();
        return c;
    }

    /// <summary>Override fields from ASTER_* environment variables.</summary>
    public void ApplyEnv()
    {
        EnvStr("ASTER_ROOT_PUBKEY_FILE", v => RootPubkeyFile = v);
        EnvStr("ASTER_ENROLLMENT_CREDENTIAL", v => EnrollmentCredentialFile = v);
        EnvStr("ASTER_ENROLLMENT_CREDENTIAL_IID", v => EnrollmentCredentialIid = v);
        EnvStr("ASTER_ENDPOINT_ADDR", v => EndpointAddr = v);
        EnvStr("ASTER_STORAGE_PATH", v => StoragePath = string.IsNullOrEmpty(v) ? null : v);
        EnvStr("ASTER_RELAY_MODE", v => RelayMode = string.IsNullOrEmpty(v) ? null : v);
        EnvStr("ASTER_BIND_ADDR", v => BindAddr = string.IsNullOrEmpty(v) ? null : v);
        EnvStr("ASTER_PORTMAPPER_CONFIG", v => PortmapperConfig = string.IsNullOrEmpty(v) ? null : v);
        EnvStr("ASTER_PROXY_URL", v => ProxyUrl = string.IsNullOrEmpty(v) ? null : v);
        EnvStr("ASTER_IDENTITY_FILE", v => IdentityFile = v);
        EnvStr("ASTER_LOG_FORMAT", v => LogFormat = v.ToLowerInvariant());
        EnvStr("ASTER_LOG_LEVEL", v => LogLevel = v.ToLowerInvariant());

        EnvBool("ASTER_ALLOW_ALL_CONSUMERS", v => AllowAllConsumers = v);
        EnvBool("ASTER_ALLOW_ALL_PRODUCERS", v => AllowAllProducers = v);
        EnvBool("ASTER_ENABLE_MONITORING", v => EnableMonitoring = v);
        EnvBool("ASTER_ENABLE_HOOKS", v => EnableHooks = v);
        EnvBool("ASTER_CLEAR_IP_TRANSPORTS", v => ClearIpTransports = v);
        EnvBool("ASTER_CLEAR_RELAY_TRANSPORTS", v => ClearRelayTransports = v);
        EnvBool("ASTER_PROXY_FROM_ENV", v => ProxyFromEnv = v);
        EnvBool("ASTER_LOCAL_DISCOVERY", v => LocalDiscovery = v);
        EnvBool("ASTER_LOG_MASK", v => LogMask = v);

        EnvInt("ASTER_HOOK_TIMEOUT_MS", v => HookTimeoutMs = v);

        EnvHex("ASTER_ROOT_PUBKEY", v => RootPubkey = v);
        EnvBase64("ASTER_SECRET_KEY", v => SecretKey = v);
    }

    /// <summary>Convert network fields to an EndpointConfigBuilder for the FFI.</summary>
    public EndpointConfigBuilder ToEndpointConfigBuilder()
    {
        var b = new EndpointConfigBuilder().Alpn(AsterServer.AsterAlpn);
        if (SecretKey != null) b.SecretKey(SecretKey);
        if (LocalDiscovery) b.EnableDiscovery(true);
        return b;
    }

    /// <summary>Resolve root public key from RootPubkey or RootPubkeyFile.</summary>
    public byte[]? ResolveRootPubkey()
    {
        if (RootPubkey != null) return RootPubkey;
        if (RootPubkeyFile != null && File.Exists(RootPubkeyFile))
        {
            var content = File.ReadAllText(RootPubkeyFile).Trim();
            if (content.Length == 64)
            {
                try { RootPubkey = Convert.FromHexString(content); return RootPubkey; }
                catch { }
            }
        }
        return null;
    }

    // ── Env helpers ──────────────────────────────────────────────────────

    private static void EnvStr(string key, Action<string> setter)
    {
        var v = Environment.GetEnvironmentVariable(key);
        if (v != null) setter(v.Trim());
    }

    private static void EnvBool(string key, Action<bool> setter)
    {
        var v = Environment.GetEnvironmentVariable(key)?.Trim().ToLowerInvariant();
        if (v is "true" or "1" or "yes" or "on") setter(true);
        else if (v is "false" or "0" or "no" or "off") setter(false);
    }

    private static void EnvInt(string key, Action<int> setter)
    {
        var v = Environment.GetEnvironmentVariable(key);
        if (v != null && int.TryParse(v.Trim(), out var n)) setter(n);
    }

    private static void EnvHex(string key, Action<byte[]> setter)
    {
        var v = Environment.GetEnvironmentVariable(key)?.Trim();
        if (!string.IsNullOrEmpty(v))
        {
            try { setter(Convert.FromHexString(v)); } catch { }
        }
    }

    private static void EnvBase64(string key, Action<byte[]> setter)
    {
        var v = Environment.GetEnvironmentVariable(key)?.Trim();
        if (!string.IsNullOrEmpty(v))
        {
            try { setter(Convert.FromBase64String(v)); } catch { }
        }
    }
}
