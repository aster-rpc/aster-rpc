using System;
using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// Status codes returned by FFI functions. Mirrors <c>aster_status_t</c>.
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
    /// <summary><c>aster_runtime_new</c> was called with a different ABI major.</summary>
    AbiMismatch = 12,
    /// <summary>Well-formed input failed a security check (envelope does not open, chain does not verify, ...).</summary>
    VerificationFailed = 13,
    /// <summary>Reserved for the R4 auth model (ABI 4.3): the caller presented no usable credential. No 4.3 function returns it.</summary>
    Unauthenticated = 14,
    /// <summary>The peer's (or local) policy refused the caller (ABI 4.3). Today only an expose relay the serving node's authorizer denied reports it.</summary>
    PermissionDenied = 15,
    /// <summary>Reserved for the R4 auth model (ABI 4.3): the presented credential has expired. No 4.3 function returns it.</summary>
    CredentialExpired = 16,
    /// <summary>ABI 4.6: <c>aster_observability_init</c> with a configuration that differs from the running pipeline's, or a second concurrent <c>aster_log_poll</c>.</summary>
    AlreadyExists = 17,
    /// <summary>ABI 4.7: the runtime's independent asynchronous-operation limit is exhausted.</summary>
    OperationLimit = 18,
}

/// <summary>
/// Event kinds emitted by the Rust completion queue via aster_poll_events.
/// Values must exactly match aster_event_kind_t in the C ABI. (Not to be confused
/// with the dispatch server's <c>aster_dispatch_event_kind_t</c>.)
/// </summary>
public enum EventKind : uint
{
    None = 0,

    // Lifecycle
    NodeCreated = 1,
    NodeCreateFailed = 2,
    /// <summary>Reserved: never emitted since ABI 4.0 removed <c>aster_endpoint_create</c>.</summary>
    EndpointCreated = 3,
    /// <summary>Reserved: never emitted since ABI 4.0.</summary>
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

    // Docs extra
    DocDel = 57,

    // Datagrams
    DatagramReceived = 60,

    // Aster custom-ALPN
    AsterAccepted = 65,

    // Hooks
    HookBeforeConnect = 70,
    HookAfterConnect = 71,
    HookInvocationReleased = 72,

    // Registry async ops (§11.9)
    RegistryResolved = 80,
    RegistryPublished = 81,
    RegistryRenewed = 82,
    RegistryAclUpdated = 83,
    RegistryAclListed = 84,

    // Generic results
    StringResult = 90,
    BytesResult = 91,
    UnitResult = 92,

    // Data plane (ABI 3.3)
    BlobFetched = 100,
    DocAuthorityReport = 101,
    DocRetireOutcome = 102,
    DocOpened = 103,
    DocEntryExists = 104,
    DocContentStatus = 105,
    StreamPriority = 106,

    // ABI 4.1 leases.
    /// <summary><c>aster_lease_acquire</c>: payload = UCv1 <c>TransitionOutcome</c>, <c>handle</c> = the lease (0 when denied).</summary>
    LeaseAcquired = 107,
    /// <summary><c>aster_lease_watch</c>: the lease left HELD; <c>flags</c> = <c>aster_lease_status_t</c>.</summary>
    LeaseStatus = 108,
    /// <summary>ABI 4.3 <c>aster_http_upload_ready</c> / <c>aster_http_response_ready</c>: <c>handle</c> = the armed handle, <c>error_code</c> = the <c>aster_http_body_status_t</c> its next non-blocking call returns, <c>status</c> = that as an <c>aster_status_t</c>.</summary>
    HttpRelayReady = 109,

    // ABI 4.4 coordination.
    /// <summary><c>aster_coordination_client_open</c>: <c>handle</c> = the client.</summary>
    CoordinationClientOpened = 110,
    /// <summary>A resolved mutation: <c>handle</c> = the resolved-result handle (0 = none), payload = UCv1 <c>D2</c>.</summary>
    CoordinationResolved = 111,
    /// <summary>A coordination read / result: payload by operation (<c>D2</c>-<c>DE</c>).</summary>
    CoordinationResult = 112,
    /// <summary><c>aster_coordination_lease_acquire</c>: granted = <c>handle</c> + <c>DD</c>, denied = handle 0 + UCv1 <c>B2</c>.</summary>
    CoordinationLeaseAcquired = 113,

    /// <summary>ABI 4.13: temporary custody protection or named retention completed.</summary>
    BlobCustodyDone = 118,

    /// <summary>ABI 4.14: ordered optional bytes from one local Docs content batch.</summary>
    DocContentsBatch = 119,
    /// <summary>ABI 4.16: owned immutable topology snapshot handle.</summary>
    TopologySnapshot = 120,
    RawWTSession = 121,
    RawWTStream = 122,
    RawWTRead = 123,
    RawWTDatagram = 124,
    RawWTClosed = 125,
    RawWTAcked = 126,

    // Errors
    OperationCancelled = 98,
    Error = 99,
}

/// <summary>
/// Configuration passed to aster_runtime_new (aster_runtime_config_t, 16 bytes).
/// struct_size must be set to the size of this struct by the caller.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct RuntimeConfig
{
    public uint struct_size;
    public uint worker_threads;
    public uint event_queue_capacity;
    public uint max_inflight_operations;

    public static RuntimeConfig Default => new()
    {
        struct_size = (uint)Marshal.SizeOf<RuntimeConfig>(),
        worker_threads = 1,
        event_queue_capacity = 256,
        max_inflight_operations = 0,
    };
}

/// <summary>
/// An event emitted by the Rust completion queue, mirroring aster_event_t (80 bytes).
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

    /// <summary>True when the pointer is NULL or the length is zero.</summary>
    public bool IsEmpty => ptr == IntPtr.Zero || len == UIntPtr.Zero;

    /// <summary>Copies the referenced bytes into a new managed array.</summary>
    public byte[] ToArray() => IsEmpty ? Array.Empty<byte>() : AsSpan().ToArray();

    /// <summary>Decodes the referenced bytes as UTF-8.</summary>
    public string ToUtf8String() => IsEmpty ? string.Empty : System.Text.Encoding.UTF8.GetString(AsSpan());

    /// <summary>Wraps a pointer that the caller keeps alive (e.g. from a <c>fixed</c> block).</summary>
    public static unsafe Bytes FromPointer(byte* p, int length)
        => length == 0 ? default : new Bytes { ptr = (IntPtr)p, len = (UIntPtr)length };

    /// <summary>
    /// Creates a Bytes from a GCHandle-pinned array.
    /// Caller must pin the array with GCHandle.Alloc(data, GCHandleType.Pinned)
    /// and free it after the FFI call.
    /// </summary>
    public static Bytes FromPinned(GCHandle handle, int length)
    {
        if (length == 0) return default; // {NULL, 0}
        return new Bytes { ptr = handle.AddrOfPinnedObject(), len = (UIntPtr)length };
    }
}

/// <summary>Relay mode for endpoint configuration. Mirrors aster_relay_mode_t.</summary>
public enum RelayMode : uint
{
    Default = 0,
    Custom = 1,
    Disabled = 2,
    Staging = 3,
}

/// <summary>Hook failure mode. Mirrors aster_hook_failure_mode_t.</summary>
public enum HookFailureMode : uint
{
    FailOpen = 0,
    FailClosed = 1,
}

/// <summary>Strongly-typed wrapper for an <see cref="Endpoint"/> handle (a node handle since ABI 4.0).</summary>
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
/// Node address used to connect to a remote peer (aster_node_addr_t, 48 bytes).
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct NodeAddr
{
    public Bytes endpoint_id;
    public Bytes relay_url;
    public BytesList direct_addresses;
}

/// <summary>
/// Configuration for aster_connect (aster_connect_config_t, 48 bytes).
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
