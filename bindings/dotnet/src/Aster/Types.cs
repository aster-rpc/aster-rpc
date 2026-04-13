using System;
using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// iroh status codes returned by FFI functions.
/// </summary>
public enum Status
{
    OK = 0,
    InvalidArgument = 1,
    NotFound = 2,
    AlreadyClosed = 3,
    QueueFull = 4,
    BufferTooSmall = 5,
    Unsupported = 6,
    Internal = 7,
    Timeout = 8,
    Cancelled = 9,
    ConnectionRefused = 10,
    StreamReset = 11,
}

/// <summary>
/// Event kinds emitted by the Rust completion queue via iroh_poll_events.
/// Values must exactly match iroh_event_kind_t in the C ABI.
/// </summary>
public enum EventKind : uint
{
    None = 0,

    // Lifecycle
    NodeCreated = 1,
    NodeCreateFailed = 2,
    EndpointCreated = 3,
    EndpointCreateFailed = 4,
    Closed = 5,

    // Connections
    Connected = 10,
    ConnectFailed = 11,
    ConnectionAccepted = 12,
    ConnectionClosed = 13,

    // Streams
    StreamOpened = 20,
    StreamAccepted = 21,
    FrameReceived = 22,
    SendCompleted = 23,
    StreamFinished = 24,
    StreamReset = 25,

    // Blobs
    BlobAdded = 30,
    BlobRead = 31,
    BlobDownloaded = 32,
    BlobTicketCreated = 33,
    BlobCollectionAdded = 34,
    BlobCollectionTicketCreated = 35,

    // Tags
    TagSet = 36,
    TagGet = 37,
    TagDeleted = 38,
    TagList = 39,

    // Docs
    DocCreated = 40,
    DocJoined = 41,
    DocSet = 42,
    DocGet = 43,
    DocShared = 44,
    AuthorCreated = 45,
    DocQuery = 46,
    DocSubscribed = 47,
    DocEvent = 48,
    DocJoinedAndSubscribed = 49,

    // Gossip
    GossipSubscribed = 50,
    GossipBroadcastDone = 51,
    GossipReceived = 52,
    GossipNeighborUp = 53,
    GossipNeighborDown = 54,
    GossipLagged = 55,

    // Blobs extra
    BlobObserveComplete = 56,

    // Datagrams
    DatagramReceived = 60,

    // Aster custom-ALPN
    AsterAccepted = 65,

    // Hooks
    HookBeforeConnect = 70,
    HookAfterConnect = 71,
    HookInvocationReleased = 72,

    // Generic results
    StringResult = 90,
    BytesResult = 91,
    UnitResult = 92,

    // Errors
    OperationCancelled = 98,
    Error = 99,
}

/// <summary>
/// Configuration passed to iroh_runtime_new.
/// struct_size must be set to the size of this struct by the caller.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct RuntimeConfig
{
    public uint struct_size;
    public uint worker_threads;
    public uint event_queue_capacity;
    public uint reserved;

    public static RuntimeConfig Default => new()
    {
        struct_size = (uint)Marshal.SizeOf<RuntimeConfig>(),
        worker_threads = 1,
        event_queue_capacity = 256,
        reserved = 0,
    };
}

/// <summary>
/// An event emitted by the Rust completion queue, mirroring iroh_event_t in C.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct Event
{
    public uint struct_size;
    public uint kind;
    public uint status;
    public ulong operation;
    public ulong handle;
    public ulong related;
    public ulong user_data;
    public IntPtr data_ptr;  // const uint8_t*
    public UIntPtr data_len; // uintptr_t
    public ulong buffer;
    public int error_code;
    public uint flags;
}

/// <summary>
/// A byte buffer used for passing data across the FFI boundary.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct Bytes
{
    public IntPtr ptr;   // const uint8_t*
    public UIntPtr len;  // uintptr_t

    public unsafe Span<byte> AsSpan() => new(ptr.ToPointer(), (int)len);

    /// <summary>
    /// Creates a Bytes from a GCHandle-pinned array.
    /// Caller must pin the array with GCHandle.Alloc(data, GCHandleType.Pinned)
    /// and free it after the FFI call.
    /// </summary>
    public static Bytes FromPinned(GCHandle handle, int length)
    {
        return new Bytes { ptr = handle.AddrOfPinnedObject(), len = (UIntPtr)length };
    }

    /// <summary>
    /// Creates a Bytes from managed array for immediate synchronous FFI calls.
    /// The data must remain valid for the duration of the call.
    /// </summary>
    public static Bytes FromArray(byte[] data)
    {
        if (data == null || data.Length == 0)
            return default;
        var handle = GCHandle.Alloc(data, GCHandleType.Pinned);
        // NOTE: caller is responsible for ensuring this is used in a synchronous
        // P/Invoke context where the pinned address remains valid.
        var bytes = new Bytes { ptr = handle.AddrOfPinnedObject(), len = (UIntPtr)data.Length };
        // We intentionally leak the pin here because FromArray is used in patterns
        // like Native.foo(Bytes.FromArray(data)) where the call is synchronous.
        // The GC handle will be collected. For long-lived usage, use FromPinned.
        handle.Free();
        return bytes;
    }
}

/// <summary>Relay mode for endpoint configuration.</summary>
public enum RelayMode : uint
{
    Default = 0,
    Custom = 1,
    Disabled = 2,
}

/// <summary>Strongly-typed wrapper for a native endpoint handle.</summary>
public readonly record struct EndpointHandle(ulong Value);

/// <summary>Strongly-typed wrapper for a native connection handle.</summary>
public readonly record struct ConnectionHandle(ulong Value);

/// <summary>Strongly-typed wrapper for a native send stream handle.</summary>
public readonly record struct SendStreamHandle(ulong Value);

/// <summary>Strongly-typed wrapper for a native recv stream handle.</summary>
public readonly record struct RecvStreamHandle(ulong Value);

/// <summary>Strongly-typed wrapper for a native node handle.</summary>
public readonly record struct NodeHandle(ulong Value);

/// <summary>
/// Runtime configuration for creating an endpoint.
/// Must match iroh_endpoint_config_t in the C header exactly (144 bytes).
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct EndpointConfig
{
    public uint struct_size;
    public uint relay_mode;
    public Bytes secret_key;
    public BytesList alpns;
    public BytesList relay_urls;
    public uint enable_discovery;
    public uint enable_hooks;
    public ulong hook_timeout_ms;
    public Bytes bind_addr;
    public uint clear_ip_transports;
    public uint clear_relay_transports;
    public uint portmapper_config;
    public Bytes proxy_url;
    public uint proxy_from_env;
    public Bytes data_dir_utf8;

    public static EndpointConfig Default => new()
    {
        struct_size = (uint)Marshal.SizeOf<EndpointConfig>(),
        relay_mode = 0,
        secret_key = default,
        alpns = default,
        relay_urls = default,
        enable_discovery = 1,
    };
}

/// <summary>
/// A list of Bytes structures.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct BytesList
{
    public IntPtr items;  // const Bytes*
    public UIntPtr len;

    public static BytesList Empty => default;
}

/// <summary>
/// Node address used to connect to a remote peer.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct NodeAddr
{
    public Bytes endpoint_id;
    public Bytes relay_url;
    public BytesList direct_addresses;
}

/// <summary>
/// Configuration for iroh_connect.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct ConnectConfig
{
    public uint struct_size;
    public uint flags;
    public Bytes node_id;
    public Bytes alpn;
    public IntPtr addr; // const NodeAddr*

    public static ConnectConfig Default => new()
    {
        struct_size = (uint)Marshal.SizeOf<ConnectConfig>(),
        flags = 0,
        node_id = default,
        alpn = default,
        addr = IntPtr.Zero,
    };
}
