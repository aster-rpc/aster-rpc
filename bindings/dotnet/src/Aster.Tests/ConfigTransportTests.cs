using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Tests;

/// <summary>ABI 5.2: aster_config_t.blob_zstd_level and reuse_last_port reach core.</summary>
public class ConfigTransportTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    /// <summary>The compressed blobs ALPN (Rust <c>aster::alpns::BLOBS_ZSTD</c>).</summary>
    private const string ZstdBlobsAlpn = "/iroh-bytes/4+zstd/1";

    [Fact]
    public unsafe void Config_Layout_Has_The_5_2_Fields()
    {
        Assert.Equal(288, sizeof(ConfigNative));
        Assert.Equal(272, Marshal.OffsetOf<ConfigNative>("storage").ToInt32());
        Assert.Equal(280, Marshal.OffsetOf<ConfigNative>("blob_zstd_level").ToInt32());
        Assert.Equal(284, Marshal.OffsetOf<ConfigNative>("reuse_last_port").ToInt32());
    }

    [Theory]
    [InlineData(null, null, 0, 0u)]   // ASTER_BLOB_ZSTD_LEVEL_DEFAULT, ASTER_REUSE_LAST_PORT_DEFAULT
    [InlineData(0, false, -1, 2u)]    // ASTER_BLOB_ZSTD_LEVEL_OFF, ASTER_REUSE_LAST_PORT_DISABLED
    [InlineData(7, true, 7, 1u)]      // ASTER_REUSE_LAST_PORT_ENABLED
    [InlineData(40, null, 19, 0u)]    // clamped like the Rust builder
    public void Config_Encodes_Level_And_Port_Reuse(int? level, bool? reuse, int wantLevel, uint wantReuse)
    {
        using var scope = new NativeScope();
        var n = new AsterConfig { BlobZstdLevel = level, ReuseLastPort = reuse }.ToNative(scope);
        Assert.Equal(wantLevel, n.blob_zstd_level);
        Assert.Equal(wantReuse, n.reuse_last_port);
    }

    [Fact]
    public void Config_Rejects_A_Negative_Level()
    {
        using var scope = new NativeScope();
        Assert.Throws<ArgumentException>(() => new AsterConfig { BlobZstdLevel = -1 }.ToNative(scope));
    }

    [Fact]
    public void Config_Defaults_Env_And_Toml_Carry_Both_Settings()
    {
        var d = AsterConfig.Default();
        Assert.Equal(3, d.BlobZstdLevel);
        Assert.True(d.ReuseLastPort);

        string path = Path.Combine(Path.GetTempPath(), $"aster-{Guid.NewGuid():N}.toml");
        File.WriteAllText(path, "[storage]\nblob_zstd_level = 9\n\n[network]\nreuse_last_port = false\n");
        try
        {
            var c = AsterConfig.FromToml(path);
            Assert.Equal(9, c.BlobZstdLevel);
            Assert.False(c.ReuseLastPort);
        }
        finally { File.Delete(path); }

        // The node knobs come from the native environment (Rust from_env); on Unix
        // Environment.SetEnvironmentVariable does not reach getenv, so use setenv.
        if (OperatingSystem.IsWindows()) return;
        setenv("ASTER_BLOB_ZSTD_LEVEL", "off", 1);
        setenv("ASTER_REUSE_LAST_PORT", "false", 1);
        try
        {
            var e = AsterConfig.FromEnv();
            Assert.Equal(0, e.BlobZstdLevel);
            Assert.False(e.ReuseLastPort);
        }
        finally
        {
            unsetenv("ASTER_BLOB_ZSTD_LEVEL");
            unsetenv("ASTER_REUSE_LAST_PORT");
        }
    }

    [DllImport("libc", SetLastError = true)]
    private static extern int setenv(string name, string value, int overwrite);

    [DllImport("libc", SetLastError = true)]
    private static extern int unsetenv(string name);

    [Fact]
    public async Task Blob_Zstd_Off_Drops_The_Zstd_Alpn()
    {
        await using var client = await Node.StartAsync(Loopback);
        foreach (var (level, served) in new (int?, bool)[] { (null, true), (0, false) })
        {
            await using var server = await Node.StartAsync(Loopback with { BlobZstdLevel = level });
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(15));
            Exception? error = null;
            try
            {
                var conn = await client.ConnectAsync(server.NodeAddr(), ZstdBlobsAlpn, cts.Token);
                conn.Dispose();
            }
            catch (Exception e) when (e is not OperationCanceledException) { error = e; }
            Assert.True(served == (error is null), $"level {level?.ToString() ?? "default"}: served={served}, dial error {error?.Message}");
        }
    }

    [Fact]
    public async Task Reuse_Last_Port_Off_Records_No_Bind_Ports()
    {
        foreach (var (reuse, recorded) in new (bool?, bool)[] { (null, true), (false, false) })
        {
            string dir = Path.Combine(Path.GetTempPath(), $"aster-reuse-{Guid.NewGuid():N}");
            try
            {
                await using (var server = await AsterServer.StartAsync(new AsterServerOptions
                {
                    Config = Loopback with { StoragePath = dir, ReuseLastPort = reuse },
                }))
                {
                }
                Assert.True(recorded == File.Exists(Path.Combine(dir, "bind-ports.json")),
                    $"ReuseLastPort={reuse?.ToString() ?? "default"}: bind-ports.json should be recorded={recorded}");
            }
            finally
            {
                if (Directory.Exists(dir)) Directory.Delete(dir, recursive: true);
            }
        }
    }
}
