using System;

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
/// These mirror iroh_event_kind_t in the C ABI.
/// </summary>
public enum EventKind : uint
{
    None = 0,

    // Connection lifecycle
    Connected = 10,
    ConnectionAccepted = 11,
    ConnectionClosed = 12,

    // Stream lifecycle
    StreamOpened = 20,
    StreamAccepted = 21,
    StreamFinished = 22,
    StreamReset = 23,

    // Data
    FrameReceived = 30,
    SendCompleted = 31,

    // Endpoint lifecycle
    EndpointCreated = 40,
    EndpointReady = 41,
    EndpointClosed = 42,

    // Gossip
    GossipReceived = 50,
    GossipNeighborUp = 53,
    GossipNeighborDown = 54,
    GossipLagged = 55,

    // Datagrams
    // TODO(iroh): DATAGRAM_RECEIVED (60) is never emitted by any FFI function.
    // iroh_connection_read_datagram emits BYTES_RESULT (91) instead.
    // Consider removing DATAGRAM_RECEIVED from this enum or clarifying its intended use.
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
        struct_size = (uint)sizeof(RuntimeConfig),
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

    public Span<byte> AsSpan() => new(ptr.ToPointer(), (int)len);
}

/// <summary>
/// Runtime configuration for creating an endpoint.
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
    public uint reserved;

    public static EndpointConfig Default => new()
    {
        struct_size = (uint)sizeof(EndpointConfig),
        relay_mode = 0, // default relay mode
        secret_key = default,
        alpns = default,
        relay_urls = default,
        enable_discovery = 1,
        reserved = 0,
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
        struct_size = (uint)sizeof(ConnectConfig),
        flags = 0,
        node_id = default,
        alpn = default,
        addr = IntPtr.Zero,
    };
}
