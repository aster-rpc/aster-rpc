using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text.Json;
using Aster.Codec;
using Aster.Rpc;

namespace Aster.Tests;

// Opt-in diagnostic witnesses. No extra decode runs inside a codec operation:
// replaying a payload there would warm the metadata cache being investigated.
internal sealed class RpcCodecDiagnostics : IDisposable
{
    private static readonly object FileLock = new();
    private readonly string _path;
    private readonly string _fixture = Guid.NewGuid().ToString("N");
    private readonly ConcurrentQueue<object> _records = new();
    private readonly List<ForyCodec> _ownedCodecs = [];
    private int _count;
    private int _disposed;

    internal static RpcCodecDiagnostics? Create() =>
        Environment.GetEnvironmentVariable("ASTER_RPC_CODEC_DIAGNOSTICS") is { Length: > 0 } path
            ? new(path) : null;

    private RpcCodecDiagnostics(string path) => _path = path;

    internal ICodec For(string role, params ServiceDefinition[] services)
    {
        var codec = new ForyCodec();
        foreach (var method in services.SelectMany(service => service.Methods.Values))
            method.RegisterTypes(codec);
        _ownedCodecs.Add(codec);
        return new RecordedCodec(this, codec, role);
    }

    private void Record<T>(string role, string operation, T value, ReadOnlySpan<byte> payload)
    {
        object? fields = value switch
        {
            EchoRequest request => new { type = "request", request.Count, request.Message, dataLength = request.Data.Length, server = "" },
            EchoReply reply => new { type = "reply", reply.Count, reply.Message, dataLength = reply.Data.Length, server = reply.ServerLang },
            _ => null,
        };
        if (fields is null || Interlocked.Increment(ref _count) > 1024) return;
        _records.Enqueue(new
        {
            fixture = _fixture, role, operation, thread = Environment.CurrentManagedThreadId,
            ticks = System.Diagnostics.Stopwatch.GetTimestamp(), fields, length = payload.Length,
            sha256 = Convert.ToHexString(SHA256.HashData(payload)),
            raw = payload.Length <= 4096 ? Convert.ToBase64String(payload) : null,
        });
    }

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        try { Save(); }
        finally
        {
            // The owner runs after client and server shutdown. Those SDK
            // lifetimes do not dispose an injected codec themselves.
            foreach (var codec in _ownedCodecs) codec.Dispose();
        }
    }

    private void Save()
    {
        lock (FileLock)
        {
            Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(_path))!);
            // Both per-fixture entries and the aggregate witness file are bounded.
            using var stream = new FileStream(_path, FileMode.Append, FileAccess.Write, FileShare.Read);
            using var writer = new StreamWriter(stream);
            foreach (var record in _records)
            {
                if (stream.Length > 64 * 1024 * 1024) break;
                writer.WriteLine(JsonSerializer.Serialize(record));
            }
        }
    }

    private sealed class RecordedCodec(RpcCodecDiagnostics owner, ForyCodec inner, string role) : ICodec
    {
        public string Mode => inner.Mode;
        public byte[] Encode<T>(T value)
        {
            byte[] payload = inner.Encode(value);
            owner.Record(role, "encode", value, payload);
            return payload;
        }
        public T Decode<T>(ReadOnlySpan<byte> payload)
        {
            T value = inner.Decode<T>(payload);
            owner.Record(role, "decode", value, payload);
            return value;
        }
    }
}
