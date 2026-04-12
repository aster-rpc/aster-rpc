using System;
using System.IO;
using System.Text;
using System.Text.Json;
using Tomlyn;  // TomlSerializer
using Tomlyn.Model;  // TomlTable

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

    /// <summary>Build config from a config file (aster.toml or aster.json), with env-var
    /// overrides (env wins). Detects format by file extension.</summary>
    public static AsterConfig FromFile(string path)
    {
        var c = new AsterConfig();
        if (path.EndsWith(".json", StringComparison.OrdinalIgnoreCase))
            c.ApplyJsonFile(path);
        else
            c.ApplyTomlFile(path);
        c.ApplyEnv();
        return c;
    }

    /// <summary>Apply values from an aster.toml file.</summary>
    public void ApplyTomlFile(string path)
    {
        var text = File.ReadAllText(path);
        var model = TomlSerializer.Deserialize<TomlTable>(text)
            ?? throw new InvalidOperationException($"Failed to parse TOML from {path}");

        void Sec(string name, Action<TomlTable> action)
        {
            if (model.TryGetValue(name, out var obj) && obj is TomlTable t) action(t);
        }

        Sec("trust", t =>
        {
            TStr(t, "root_pubkey_file", v => RootPubkeyFile = v);
            TStr(t, "enrollment_credential", v => EnrollmentCredentialFile = v);
            TStr(t, "enrollment_credential_iid", v => EnrollmentCredentialIid = v);
            TBool(t, "allow_all_consumers", v => AllowAllConsumers = v);
            TBool(t, "allow_all_producers", v => AllowAllProducers = v);
            TStr(t, "root_pubkey", v => RootPubkey = Convert.FromHexString(v));
        });

        Sec("connect", t => TStr(t, "endpoint_addr", v => EndpointAddr = v));
        Sec("storage", t => TStr(t, "path", v => StoragePath = v));

        Sec("network", t =>
        {
            TStr(t, "relay_mode", v => RelayMode = v);
            TStr(t, "bind_addr", v => BindAddr = v);
            TStr(t, "portmapper_config", v => PortmapperConfig = v);
            TStr(t, "proxy_url", v => ProxyUrl = v);
            TBool(t, "enable_monitoring", v => EnableMonitoring = v);
            TBool(t, "enable_hooks", v => EnableHooks = v);
            TBool(t, "clear_ip_transports", v => ClearIpTransports = v);
            TBool(t, "clear_relay_transports", v => ClearRelayTransports = v);
            TBool(t, "proxy_from_env", v => ProxyFromEnv = v);
            TBool(t, "local_discovery", v => LocalDiscovery = v);
            if (t.TryGetValue("hook_timeout_ms", out var htm) && htm is long htmVal)
                HookTimeoutMs = (int)htmVal;
            TStr(t, "secret_key", v => SecretKey = Convert.FromBase64String(v));
        });

        Sec("logging", t =>
        {
            TStr(t, "format", v => LogFormat = v.ToLowerInvariant());
            TStr(t, "level", v => LogLevel = v.ToLowerInvariant());
            TBool(t, "mask", v => LogMask = v);
        });
    }

    private static void TStr(TomlTable t, string k, Action<string> s) { if (t.TryGetValue(k, out var v) && v is string sv) s(sv); }
    private static void TBool(TomlTable t, string k, Action<bool> s) { if (t.TryGetValue(k, out var v) && v is bool bv) s(bv); }

    /// <summary>Apply values from a JSON config file (aster.json).
    /// The file uses the same section structure as aster.toml but in JSON form:
    /// {"trust": {...}, "connect": {...}, "storage": {...}, "network": {...}, "logging": {...}}</summary>
    public void ApplyJsonFile(string path)
    {
        var text = File.ReadAllText(path);
        using var doc = JsonDocument.Parse(text);
        var root = doc.RootElement;

        // trust section
        if (root.TryGetProperty("trust", out var trust))
        {
            JsonStr(trust, "root_pubkey_file", v => RootPubkeyFile = v);
            JsonStr(trust, "enrollment_credential", v => EnrollmentCredentialFile = v);
            JsonStr(trust, "enrollment_credential_iid", v => EnrollmentCredentialIid = v);
            JsonBool(trust, "allow_all_consumers", v => AllowAllConsumers = v);
            JsonBool(trust, "allow_all_producers", v => AllowAllProducers = v);
            JsonStr(trust, "root_pubkey", v => RootPubkey = Convert.FromHexString(v));
        }

        // connect section
        if (root.TryGetProperty("connect", out var conn))
            JsonStr(conn, "endpoint_addr", v => EndpointAddr = v);

        // storage section
        if (root.TryGetProperty("storage", out var stor))
            JsonStr(stor, "path", v => StoragePath = v);

        // network section
        if (root.TryGetProperty("network", out var net))
        {
            JsonStr(net, "relay_mode", v => RelayMode = v);
            JsonStr(net, "bind_addr", v => BindAddr = v);
            JsonStr(net, "portmapper_config", v => PortmapperConfig = v);
            JsonStr(net, "proxy_url", v => ProxyUrl = v);
            JsonBool(net, "enable_monitoring", v => EnableMonitoring = v);
            JsonBool(net, "enable_hooks", v => EnableHooks = v);
            JsonBool(net, "clear_ip_transports", v => ClearIpTransports = v);
            JsonBool(net, "clear_relay_transports", v => ClearRelayTransports = v);
            JsonBool(net, "proxy_from_env", v => ProxyFromEnv = v);
            JsonBool(net, "local_discovery", v => LocalDiscovery = v);
            JsonInt(net, "hook_timeout_ms", v => HookTimeoutMs = v);
            JsonStr(net, "secret_key", v => SecretKey = Convert.FromBase64String(v));
        }

        // logging section
        if (root.TryGetProperty("logging", out var log))
        {
            JsonStr(log, "format", v => LogFormat = v.ToLowerInvariant());
            JsonStr(log, "level", v => LogLevel = v.ToLowerInvariant());
            JsonBool(log, "mask", v => LogMask = v);
        }
    }

    private static void JsonStr(JsonElement e, string key, Action<string> setter)
    {
        if (e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.String)
            setter(v.GetString()!);
    }

    private static void JsonBool(JsonElement e, string key, Action<bool> setter)
    {
        if (e.TryGetProperty(key, out var v) && (v.ValueKind == JsonValueKind.True || v.ValueKind == JsonValueKind.False))
            setter(v.GetBoolean());
    }

    private static void JsonInt(JsonElement e, string key, Action<int> setter)
    {
        if (e.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.Number)
            setter(v.GetInt32());
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
