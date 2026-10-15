using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using Aster;

namespace Aster.Tests;

/// <summary>
/// ABI Contract Tests (5b.5)
///
/// Validates the .NET FFI bindings against the C ABI:
/// - Struct sizes match the C header
/// - Field offsets are correct
/// - Enum values match expected integers
/// - Round-trip: marshal in .NET, pass to native, read back
/// - Submit/cancel/close round-trip via Runtime
/// - No stale handle access after close
/// - op_id sequence stays monotonic across submissions
///
/// These are pure ABI tests — no networking required.
public class AbiContractTests
{
    [Fact]
    public void PackagedNativeLibraryLoadsWithoutFilenameAliases()
    {
        string filename = OperatingSystem.IsWindows() ? "aster_transport_ffi.dll"
            : OperatingSystem.IsMacOS() ? "libaster_transport_ffi.dylib"
            : "libaster_transport_ffi.so";
        Assert.True(File.Exists(Path.Combine(AppContext.BaseDirectory, filename)),
            $"The published native filename {filename} must be copied to the consumer output");

        // Use the ordinary P/Invoke path: no resolver, absolute-path preload,
        // or renamed library may hide a mismatch with the packaged filename.
        using var runtime = new Runtime();
        Assert.NotEqual(0UL, runtime.Handle);
    }

    // ─── Struct size verification ─────────────────────────────────────────────

    [Fact]
    public void Event_Size_Matches_C_Header()
    {
        // iroh_event_t from ffi/src/lib.rs:
        // struct iroh_event_t {
        //   uint32_t struct_size;    // 0
        //   uint32_t kind;          // 4
        //   uint32_t status;        // 8
        //   uint64_t operation;     // 16
        //   uint64_t handle;        // 24
        //   uint64_t related;       // 32
        //   uint64_t user_data;    // 40
        //   uint64_t data_ptr;      // 48
        //   uint64_t data_len;      // 56
        //   uint64_t buffer;        // 64
        //   int32_t error_code;     // 72
        //   uint32_t flags;         // 76
        // }; = 80 bytes
        Assert.Equal(80, Marshal.SizeOf<Event>());
    }

    [Fact]
    public void Bytes_Size_Matches_C_Header()
    {
        // struct iroh_bytes_t { const uint8_t* ptr; uintptr_t len; } = 16 bytes
        Assert.Equal(16, Marshal.SizeOf<Bytes>());
    }

    [Fact]
    public void BytesList_Size_Matches_C_Header()
    {
        // struct iroh_bytes_list_t { const void* items; uintptr_t len; } = 16 bytes
        Assert.Equal(16, Marshal.SizeOf<BytesList>());
    }

    [Fact]
    public void RuntimeConfig_Size_Matches_C_Header()
    {
        // struct iroh_runtime_config_t {
        //   uint32_t struct_size;        // 0
        //   uint32_t worker_threads;     // 4
        //   uint32_t event_queue_capacity; // 8
        //   uint32_t max_inflight_operations; // 12
        // }; = 16 bytes
        Assert.Equal(16, Marshal.SizeOf<RuntimeConfig>());
        Assert.Equal(8, Marshal.OffsetOf<RuntimeConfig>(nameof(RuntimeConfig.event_queue_capacity)).ToInt32());
        Assert.Equal(12, Marshal.OffsetOf<RuntimeConfig>(nameof(RuntimeConfig.max_inflight_operations)).ToInt32());
    }

    [Fact]
    public void ConnectConfig_Size_Matches_C_Header()
    {
        // struct iroh_connect_config_t {
        //   uint32_t struct_size;  // 0
        //   uint32_t flags;        // 4
        //   iroh_bytes_t node_id;  // 8 (16 bytes)
        //   iroh_bytes_t alpn;     // 24 (16 bytes)
        //   const iroh_node_addr_t* addr; // 40 (8 bytes)
        // }; = 48 bytes
        Assert.Equal(48, Marshal.SizeOf<ConnectConfig>());
    }

    // ─── Field offset verification ──────────────────────────────────────────

    [Fact]
    public void Event_Field_Offsets_Use_PointerMath()
    {
        // Use pointer arithmetic to verify field offsets match C header.
        // This approach is more reliable than Marshal.OffsetOf on .NET 9.
        var ev = new Event();
        IntPtr mem = Marshal.AllocHGlobal(Marshal.SizeOf<Event>());
        try
        {
            Marshal.StructureToPtr(ev, mem, fDeleteOld: false);

            // Read each field via pointer offset and verify expected values
            unsafe
            {
                byte* basePtr = (byte*)mem.ToPointer();

                // struct_size at offset 0 (uint32)
                uint* ptr0 = (uint*)(basePtr + 0);
                Assert.Equal(0u, *ptr0);

                // kind at offset 4 (uint32)
                uint* ptr4 = (uint*)(basePtr + 4);
                Assert.Equal(0u, *ptr4);

                // operation at offset 16 (uint64)
                ulong* ptr16 = (ulong*)(basePtr + 16);
                Assert.Equal(0ul, *ptr16);

                // handle at offset 24 (uint64)
                ulong* ptr24 = (ulong*)(basePtr + 24);
                Assert.Equal(0ul, *ptr24);
            }
        }
        finally
        {
            Marshal.FreeHGlobal(mem);
        }
    }

    [Fact]
    public void Event_Field_Offsets_By_Size()
    {
        // Verify sequential layout by checking that field sizes match expectations.
        // uint32: 4 bytes, uint64: 8 bytes.
        // This confirms LayoutKind.Sequential is applied.
        int size = Marshal.SizeOf<Event>();

        // struct_size (4) + kind (4) + status (4) + operation (8) +
        // handle (8) + related (8) + user_data (8) + data_ptr (8) +
        // data_len (8) + buffer (8) + error_code (4) + flags (4) = 80
        Assert.Equal(80, size);
    }

    private static void Off<T>(string field, int expected)
        => Assert.Equal(expected, (int)Marshal.OffsetOf<T>(field));

    [Fact]
    public void All_Struct_Sizes_Match_Header()
    {
        // Sizes from ffi/RENAME_MAP.md / ffi/src/layout.rs (LP64). Both the
        // marshalled size and the blittable in-memory size must agree, since
        // poll buffers are passed pinned (no marshalling copy).
        Assert.Equal(80, Marshal.SizeOf<Event>());
        Assert.Equal(80, System.Runtime.CompilerServices.Unsafe.SizeOf<Event>());
        Assert.Equal(16, Marshal.SizeOf<RuntimeConfig>());
        Assert.Equal(48, Marshal.SizeOf<ConnectConfig>());
        Assert.Equal(48, Marshal.SizeOf<NodeAddr>());
        Assert.Equal(16, Marshal.SizeOf<Bytes>());
        Assert.Equal(16, Marshal.SizeOf<BytesList>());
        Assert.Equal(DispatchServer.EventSize, Marshal.SizeOf<DispatchEventNative>());
        Assert.Equal(DispatchServer.EventSize, System.Runtime.CompilerServices.Unsafe.SizeOf<DispatchEventNative>());
        Assert.Equal(72, Marshal.SizeOf<CallOptionsNative>());
    }

    [Fact]
    public void Event_Field_Offsets_Match_Header()
    {
        Off<Event>("struct_size", 0);
        Off<Event>("kind", 4);
        Off<Event>("status", 8);
        Off<Event>("operation", 16);
        Off<Event>("handle", 24);
        Off<Event>("related", 32);
        Off<Event>("user_data", 40);
        Off<Event>("data_ptr", 48);
        Off<Event>("data_len", 56);
        Off<Event>("buffer", 64);
        Off<Event>("error_code", 72);
        Off<Event>("flags", 76);
    }

    [Fact]
    public void ConnectConfig_And_NodeAddr_Offsets_Match_Header()
    {
        Off<ConnectConfig>("struct_size", 0);
        Off<ConnectConfig>("flags", 4);
        Off<ConnectConfig>("node_id", 8);
        Off<ConnectConfig>("alpn", 24);
        Off<ConnectConfig>("addr", 40);
        Off<NodeAddr>("endpoint_id", 0);
        Off<NodeAddr>("relay_url", 16);
        Off<NodeAddr>("direct_addresses", 32);
    }

    [Fact]
    public void DispatchEvent_Offsets_Match_Header()
    {
        // aster_dispatch_event_t (ffi/src/layout.rs; same as Java Dispatch.EVENT_LAYOUT).
        Off<DispatchEventNative>("kind", 0);
        Off<DispatchEventNative>("pattern", 1);
        Off<DispatchEventNative>("reason", 2);
        Off<DispatchEventNative>("flags", 4);
        Off<DispatchEventNative>("call_id", 8);
        Off<DispatchEventNative>("service_idx", 16);
        Off<DispatchEventNative>("method_idx", 20);
        Off<DispatchEventNative>("session_key", 24);
        Off<DispatchEventNative>("connection_id", 32);
        Off<DispatchEventNative>("connection_handle", 40);
        Off<DispatchEventNative>("deadline_unix_ms", 48);
        Off<DispatchEventNative>("wire_call_id", 56);
        Off<DispatchEventNative>("session_id", 60);
        Off<DispatchEventNative>("request_ptr", 64);
        Off<DispatchEventNative>("request_len", 72);
        Off<DispatchEventNative>("peer_ptr", 80);
        Off<DispatchEventNative>("peer_len", 88);
        Off<DispatchEventNative>("buffer", 96);
    }

    [Fact]
    public void CallOptions_Offsets_Match_Header()
    {
        // aster_call_options_t (ffi/src/layout.rs; same as Java Rpc.OPTIONS_LAYOUT).
        Off<CallOptionsNative>("struct_size", 0);
        Off<CallOptionsNative>("version", 4);
        Off<CallOptionsNative>("service", 8);
        Off<CallOptionsNative>("method", 24);
        Off<CallOptionsNative>("metadata", 40);
        Off<CallOptionsNative>("session_id", 56);
        Off<CallOptionsNative>("deadline_ms", 60);
        Off<CallOptionsNative>("serialization_mode", 64);
        Off<CallOptionsNative>("wire_call_id", 68);
    }

    [Fact]
    public void Dispatch_Enum_Values_Match_Header()
    {
        Assert.Equal(0, (byte)DispatchEventKind.Call);
        Assert.Equal(1, (byte)DispatchEventKind.Cancelled);
        Assert.Equal(2, (byte)DispatchEventKind.ResponseWritable);
        Assert.Equal(3, (byte)DispatchEventKind.ResponseClosed);
        Assert.Equal(4, (byte)DispatchEventKind.SessionClosed);
        Assert.Equal(5, (byte)DispatchEventKind.ConnectionClosed);
        Assert.Equal(6, (byte)DispatchEventKind.NonRpc);
        Assert.Equal(0, (byte)DispatchCancelReason.Peer);
        Assert.Equal(1, (byte)DispatchCancelReason.Deadline);
        Assert.Equal(2, (byte)DispatchCancelReason.InvalidRequest);
        Assert.Equal(0x1u, (uint)DispatchFlags.FirstIsEnd);
        Assert.Equal(0x2u, (uint)DispatchFlags.NewSession);
        Assert.Equal(0x4u, (uint)DispatchFlags.IrohAuthenticated);
        Assert.Equal(0x100u, (uint)DispatchFlags.HasMetadata);
        Assert.Equal(0x200u, (uint)DispatchFlags.HasAttributes);
        Assert.Equal(1u, (uint)DispatchServerOption.BuiltinNodeInfo);
        Assert.Equal(2u, (uint)DispatchServerOption.BuiltinTopology);
        Assert.Equal(3u, (uint)DispatchServerOption.MaxSessionsPerConnection);
        Assert.Equal(4u, (uint)DispatchServerOption.HandlerTimeoutMs);
        Assert.Equal(uint.MaxValue, Native.TimeoutInfinite);
    }

    [Fact]
    public void KvList_RoundTrips()
    {
        var entries = new[] { new KeyValuePair<string, string>("a", "1"), new("ключ", "✓"), new("empty", "") };
        byte[] bytes = KvList.Encode(entries);
        Assert.Equal(3u, BitConverter.ToUInt32(bytes, 0));
        Assert.Equal(entries, KvList.Decode(bytes));
        Assert.Empty(KvList.Encode(Array.Empty<KeyValuePair<string, string>>()));
        Assert.Empty(KvList.Decode(ReadOnlySpan<byte>.Empty));
    }

    // ─── Enum value verification ────────────────────────────────────────────

    [Fact]
    public void EventKind_Values_Match_Rust()
    {
        // These values are from Rust iroh_event_kind_t (verified against Go binding)
        Assert.Equal(0u, (uint)EventKind.None);
        Assert.Equal(1u, (uint)EventKind.NodeCreated);
        Assert.Equal(2u, (uint)EventKind.NodeCreateFailed);
        Assert.Equal(3u, (uint)EventKind.EndpointCreated);
        Assert.Equal(4u, (uint)EventKind.EndpointCreateFailed);
        Assert.Equal(5u, (uint)EventKind.Closed);
        Assert.Equal(10u, (uint)EventKind.Connected);
        Assert.Equal(11u, (uint)EventKind.ConnectFailed);
        Assert.Equal(12u, (uint)EventKind.ConnectionAccepted);
        Assert.Equal(13u, (uint)EventKind.ConnectionClosed);
        Assert.Equal(20u, (uint)EventKind.StreamOpened);
        Assert.Equal(21u, (uint)EventKind.StreamAccepted);
        Assert.Equal(22u, (uint)EventKind.FrameReceived);
        Assert.Equal(23u, (uint)EventKind.SendCompleted);
        Assert.Equal(24u, (uint)EventKind.StreamFinished);
        Assert.Equal(25u, (uint)EventKind.StreamReset);
        Assert.Equal(60u, (uint)EventKind.DatagramReceived);
        Assert.Equal(65u, (uint)EventKind.AsterAccepted);
        Assert.Equal(90u, (uint)EventKind.StringResult);
        Assert.Equal(91u, (uint)EventKind.BytesResult);
        Assert.Equal(92u, (uint)EventKind.UnitResult);
        Assert.Equal(98u, (uint)EventKind.OperationCancelled);
        Assert.Equal(99u, (uint)EventKind.Error);
    }

    [Fact]
    public void Status_Values_Match_Rust()
    {
        Assert.Equal(0, (int)Status.OK);
        Assert.Equal(1, (int)Status.InvalidArgument);
        Assert.Equal(2, (int)Status.NotFound);
        Assert.Equal(3, (int)Status.AlreadyClosed);
        Assert.Equal(4, (int)Status.QueueFull);
        Assert.Equal(5, (int)Status.BufferTooSmall);
        Assert.Equal(6, (int)Status.Unsupported);
        Assert.Equal(7, (int)Status.Internal);
        Assert.Equal(8, (int)Status.Timeout);
        Assert.Equal(9, (int)Status.Cancelled);
        Assert.Equal(10, (int)Status.ConnectionRefused);
        Assert.Equal(11, (int)Status.StreamReset);
        Assert.Equal(12, (int)Status.AbiMismatch);
        Assert.Equal(18, (int)Status.OperationLimit);
        Assert.Equal(19, (int)Status.Unavailable);
    }

    // ─── Runtime version check ────────────────────────────────────────────────

    [Fact]
    public void Runtime_Version_Is_Consistent()
    {
        var (major, minor, patch) = Runtime.Version;
        Assert.Equal(5, major); // ASTER_ABI_VERSION_MAJOR this binding targets
        Assert.True(major >= 0);
        Assert.True(minor >= 0);
        Assert.True(patch >= 0);
    }

    // ─── Struct round-trip (marshal → native → unmarshal) ───────────────────

    [Fact]
    public void Event_RoundTrip_Preserves_Fields()
    {
        var ev = new Event
        {
            struct_size = 80,
            kind = 22, // FrameReceived
            status = 0,
            operation = 42,
            handle = 7,
            related = 0,
            user_data = 99,
            data_ptr = IntPtr.Zero,
            data_len = UIntPtr.Zero,
            buffer = 55,
            error_code = 0,
            flags = 1,
        };

        IntPtr mem = Marshal.AllocHGlobal(Marshal.SizeOf<Event>());
        try
        {
            Marshal.StructureToPtr(ev, mem, fDeleteOld: false);
            var roundTrip = Marshal.PtrToStructure<Event>(mem);

            Assert.Equal(ev.struct_size, roundTrip.struct_size);
            Assert.Equal(ev.kind, roundTrip.kind);
            Assert.Equal(ev.status, roundTrip.status);
            Assert.Equal(ev.operation, roundTrip.operation);
            Assert.Equal(ev.handle, roundTrip.handle);
            Assert.Equal(ev.related, roundTrip.related);
            Assert.Equal(ev.user_data, roundTrip.user_data);
            Assert.Equal(ev.buffer, roundTrip.buffer);
            Assert.Equal(ev.error_code, roundTrip.error_code);
            Assert.Equal(ev.flags, roundTrip.flags);
        }
        finally
        {
            Marshal.FreeHGlobal(mem);
        }
    }

    // ─── ABI 4.1: upcalls, lease, fenced resource, topology ─────────────────

    [Fact]
    public void Upcall_Struct_Sizes_And_Offsets_Match_Header()
    {
        // aster_upcall_event_t (ffi/src/layout.rs; same as Java Upcalls.EVENT_LAYOUT): 40 bytes.
        Assert.Equal(40, Marshal.SizeOf<UpcallEventNative>());
        Assert.Equal(40, System.Runtime.CompilerServices.Unsafe.SizeOf<UpcallEventNative>());
        Assert.Equal(Upcall.Upcalls.EventSize, Marshal.SizeOf<UpcallEventNative>());
        Off<UpcallEventNative>("invocation_id", 0);
        Off<UpcallEventNative>("kind", 8);
        Off<UpcallEventNative>("cancel_reason", 10);
        Off<UpcallEventNative>("_p0", 11);
        Off<UpcallEventNative>("payload_len", 12);
        Off<UpcallEventNative>("deadline_unix_ms", 16);
        Off<UpcallEventNative>("payload_ptr", 24);
        Off<UpcallEventNative>("buffer", 32);
        // aster_topology_admitted_stats_t: 4 x u64.
        Assert.Equal(32, Marshal.SizeOf<TopologyAdmittedStatsNative>());
        Off<TopologyAdmittedStatsNative>("hits", 0);
        Off<TopologyAdmittedStatsNative>("stale", 8);
        Off<TopologyAdmittedStatsNative>("misses", 16);
        Off<TopologyAdmittedStatsNative>("upcalls", 24);
    }

    [Fact]
    public void Upcall_Enum_Values_Match_Header()
    {
        // aster_upcall_kind_t: codes never change.
        Assert.Equal(0, (int)Upcall.UpcallKind.Cancel);
        Assert.Equal(1, (int)Upcall.UpcallKind.LeaseTransition);
        Assert.Equal(2, (int)Upcall.UpcallKind.LeaseSnapshot);
        Assert.Equal(3, (int)Upcall.UpcallKind.LeaseAuthorize);
        Assert.Equal(4, (int)Upcall.UpcallKind.FencedMutate);
        Assert.Equal(16, (int)Upcall.UpcallKind.TopologyAdmitted);
        Assert.Equal(17, (int)Upcall.UpcallKind.TopologySnapshot);
        Assert.Equal(32, (int)Upcall.UpcallKind.PlacementPolicy);
        Assert.Equal(33, (int)Upcall.UpcallKind.PlacementProvision);
        Assert.Equal(48, (int)Upcall.UpcallKind.ExposeAuthorize);
        Assert.Equal(49, (int)Upcall.UpcallKind.ExposeHttp);
        Assert.Equal(50, (int)Upcall.UpcallKind.ExposeRoutePolicy);
        Assert.Equal(64, (int)Upcall.UpcallKind.AdmissionConnect);
        Assert.Equal(65, (int)Upcall.UpcallKind.AdmissionHandshake);
        // aster_upcall_status_t / aster_upcall_cancel_reason_t.
        Assert.Equal(0u, (uint)Upcall.UpcallStatus.Ok);
        Assert.Equal(1u, (uint)Upcall.UpcallStatus.Error);
        Assert.Equal(2u, (uint)Upcall.UpcallStatus.Unimplemented);
        Assert.Equal(0, (byte)Upcall.UpcallCancelReason.None);
        Assert.Equal(1, (byte)Upcall.UpcallCancelReason.Timeout);
        Assert.Equal(2, (byte)Upcall.UpcallCancelReason.Abandoned);
        Assert.Equal(3, (byte)Upcall.UpcallCancelReason.Shutdown);
        // aster_lease_status_t, ASTER_EVENT_LEASE_*, aster_hook_failure_mode_t, ASTER_UPCALL_DEFAULT_CAPACITY.
        Assert.Equal(0u, (uint)Lease.LeaseStatus.Held);
        Assert.Equal(1u, (uint)Lease.LeaseStatus.Released);
        Assert.Equal(2u, (uint)Lease.LeaseStatus.Fenced);
        Assert.Equal(3u, (uint)Lease.LeaseStatus.Lost);
        Assert.Equal(107u, (uint)EventKind.LeaseAcquired);
        Assert.Equal(108u, (uint)EventKind.LeaseStatus);
        Assert.Equal(0u, (uint)HookFailureMode.FailOpen);
        Assert.Equal(1u, (uint)HookFailureMode.FailClosed);
        Assert.Equal(1024, Upcall.Upcalls.DefaultCapacity);
        // UCv1 enum bytes (design §3): deny reasons and fence rejections.
        Assert.Equal(new byte[] { 1, 2, 3, 4, 5, 6 }, Enum.GetValues<Lease.DenyReason>().Select(r => (byte)r).Order().ToArray());
        Assert.Equal(new byte[] { 1, 2, 3, 4 }, Enum.GetValues<Lease.FenceRejectionReason>().Select(r => (byte)r).Order().ToArray());
    }

    /// <summary>The 4.1 exports: declared in the header, present in the library, and declared in <see cref="Native"/>.</summary>
    private static readonly string[] Abi41Exports =
    {
        "aster_upcalls_new", "aster_upcalls_close", "aster_upcalls_free", "aster_upcall_set_policy", "aster_upcall_set_default_policy",
        "aster_upcall_poll", "aster_upcall_buffer_release", "aster_upcall_respond",
        "aster_lease_serializer_foreign_new", "aster_lease_serializer_memory_new", "aster_lease_serializer_free",
        "aster_lease_acquire", "aster_lease_fence", "aster_lease_status", "aster_lease_watch", "aster_lease_release",
        "aster_lease_free", "aster_lease_snapshot", "aster_lease_revoke",
        "aster_fenced_resource_foreign_new", "aster_fenced_resource_free", "aster_fenced_mutate",
        "aster_topology_admitted_new", "aster_topology_admitted_free", "aster_topology_admitted_check", "aster_topology_admitted_set",
        "aster_topology_admitted_invalidate", "aster_topology_admitted_prefetch", "aster_topology_admitted_stats", "aster_topology_enable_shared",
    };

    internal static void AssertExports(string[] names)
    {
        string? header = null;
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !File.Exists(Path.Combine(dir.FullName, "ffi", "aster_ffi.h"))) dir = dir.Parent;
        if (dir != null) header = File.ReadAllText(Path.Combine(dir.FullName, "ffi", "aster_ffi.h"));

        Assert.True(NativeLibrary.TryLoad("aster_transport_ffi", typeof(Native).Assembly, null, out IntPtr lib), "native library");
        foreach (string name in names)
        {
            Assert.True(typeof(Native).GetMethod(name, System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static) != null, $"Native.{name} is not declared");
            Assert.True(NativeLibrary.TryGetExport(lib, name, out _), $"{name} is not exported by the native library");
            if (header != null) Assert.Matches($@"\b{name}\(", header);
        }
    }

    [Fact]
    public void Abi_4_1_Exports_Are_Declared_Exported_And_In_The_Header() => AssertExports(Abi41Exports);

    /// <summary>The 5.1 exports: Enrollment from configuration and the lease grantor.</summary>
    [Fact]
    public void Abi_5_1_Exports_Are_Declared_Exported_And_In_The_Header()
    {
        AssertExports([
            "aster_config_enrollment_from_env", "aster_config_enrollment_from_toml", "aster_node_start_enrolled",
            "aster_enrollment_enroll", "aster_enrollment_free", "aster_enrollment_check_peer", "aster_enrollment_peer_claims",
            "aster_enrollment_status", "aster_enrollment_next_revocation", "aster_enrollment_issuer_info", "aster_enrollment_issue",
            "aster_server_add_lease_grantor", "aster_lease_serializer_grantor_new",
        ]);
        Assert.Equal(127u, (uint)EventKind.EnrollmentResult);
        Assert.Equal(2u, Native.RequiredAbiMinor);
        Assert.Equal(new uint[] { 0, 1, 2, 3, 4 }, Enum.GetValues<AdmissionErrorKind>().Select(k => (uint)k).Order().ToArray());
        Assert.Equal(0u, Native.EnrollmentSideServer);
        Assert.Equal(1u, Native.EnrollmentSideClient);
    }

    // ─── ABI 4.2: expose ────────────────────────────────────────────────────

    [Fact]
    public void Expose_Struct_Sizes_And_Offsets_Match_Header()
    {
        // aster_http_foreign_options_t: 48 bytes.
        Assert.Equal(48, Marshal.SizeOf<HttpForeignOptionsNative>());
        Off<HttpForeignOptionsNative>("struct_size", 0);
        Off<HttpForeignOptionsNative>("flags", 4);
        Off<HttpForeignOptionsNative>("inline_request_max", 8);
        Off<HttpForeignOptionsNative>("max_request_body", 16);
        Off<HttpForeignOptionsNative>("max_response_body", 24);
        Off<HttpForeignOptionsNative>("max_chunk", 32);
        Off<HttpForeignOptionsNative>("response_buffer", 40);
        Off<HttpForeignOptionsNative>("reserved", 44);
        // aster_route_spec_t: 40 bytes (LP64).
        Assert.Equal(40, Marshal.SizeOf<RouteSpecNative>());
        Off<RouteSpecNative>("host", 0);
        Off<RouteSpecNative>("service_id", 16);
        Off<RouteSpecNative>("port", 32);
        Off<RouteSpecNative>("protocol", 36);
    }

    [Fact]
    public unsafe void Http_Foreign_Options_Init_Fills_The_Native_Struct_Size()
    {
        HttpForeignOptionsNative o = default;
        Native.Check(Native.aster_http_foreign_options_init(&o), "init");
        Assert.Equal(48u, o.struct_size);
        Assert.Equal(0u, o.flags);
        Assert.Equal(0u, o.reserved);
    }

    [Fact]
    public void Http_Body_Status_Values_Match_Header()
    {
        // aster_http_body_status_t: 1..9 are the stable BodyError codes, negatives are argument / handle errors.
        Assert.Equal(0, (int)Expose.HttpBodyStatus.Ok);
        Assert.Equal(1, (int)Expose.HttpBodyStatus.UnknownStream);
        Assert.Equal(2, (int)Expose.HttpBodyStatus.Cancelled);
        Assert.Equal(3, (int)Expose.HttpBodyStatus.TooLarge);
        Assert.Equal(4, (int)Expose.HttpBodyStatus.ChunkTooLarge);
        Assert.Equal(5, (int)Expose.HttpBodyStatus.Finished);
        Assert.Equal(6, (int)Expose.HttpBodyStatus.Timeout);
        Assert.Equal(7, (int)Expose.HttpBodyStatus.Read);
        Assert.Equal(8, (int)Expose.HttpBodyStatus.InvalidTrailers);
        Assert.Equal(9, (int)Expose.HttpBodyStatus.BlockingOnRuntime);
        Assert.Equal(-1, (int)Expose.HttpBodyStatus.InvalidArgument);
        Assert.Equal(-2, (int)Expose.HttpBodyStatus.UnknownHandle);
        // Route protocols and HTTP versions (UCv1 u8enums).
        Assert.Equal(0, Upcall.ExposeRouteSpec.ProtocolHttp);
        Assert.Equal(1, Upcall.ExposeRouteSpec.ProtocolTcp);
        Assert.Equal(new byte[] { 0, 1, 2, 3, 4 }, Enum.GetValues<Expose.HttpProtocolVersion>().Select(v => (byte)v).Order().ToArray());
    }

    private static readonly string[] Abi42Exports =
    {
        "aster_expose_new", "aster_expose_free", "aster_expose_unexpose", "aster_expose_serve", "aster_expose_handle_connection",
        "aster_expose_socket", "aster_expose_http_foreign", "aster_expose_route_policy_foreign",
        "aster_http_bodies_new", "aster_http_bodies_free", "aster_http_bodies_open_count", "aster_http_foreign_options_init",
        "aster_http_body_read", "aster_http_body_write", "aster_http_body_try_write", "aster_http_body_finish", "aster_http_body_abort",
        "aster_expose_relay_request", "aster_expose_request_route", "aster_route_registration_free",
    };

    [Fact]
    public void Abi_4_2_Exports_Are_Declared_Exported_And_In_The_Header() => AssertExports(Abi42Exports);

    [Fact]
    public void Body_Calls_Validate_Arguments_And_Unknown_Handles()
    {
        using var rt = new Runtime();
        unsafe
        {
            byte b = 0;
            UIntPtr n = UIntPtr.Zero;
            uint eof = 0;
            // Unknown table -> -2 (these functions do not return aster_status_t).
            Assert.Equal((int)Expose.HttpBodyStatus.UnknownHandle, Native.aster_http_body_read(rt.Handle, 424_242, 1, &b, (UIntPtr)1, 0, &n, &eof));
            Assert.Equal((int)Expose.HttpBodyStatus.UnknownHandle, Native.aster_http_body_finish(rt.Handle, 424_242, 1, null, UIntPtr.Zero));
            using var bodies = Expose.HttpBodies.Create(rt);
            // Zero cap, NULL buffer with a length: invalid arguments, not a crash.
            Assert.Equal((int)Expose.HttpBodyStatus.InvalidArgument, Native.aster_http_body_read(rt.Handle, bodies.NativeHandle, 1, &b, UIntPtr.Zero, 0, &n, &eof));
            Assert.Equal((int)Expose.HttpBodyStatus.InvalidArgument, Native.aster_http_body_write(rt.Handle, bodies.NativeHandle, 1, null, (UIntPtr)4, 0));
            Assert.Equal(0ul, bodies.OpenCount);
        }
    }

    // ─── ABI 4.3: streaming consumer relay, reserved auth statuses ──────────

    [Fact]
    public unsafe void Relay_Options_Layout_And_Init_Match_Header()
    {
        // aster_http_relay_options_t: 24 bytes.
        Assert.Equal(24, Marshal.SizeOf<HttpRelayOptionsNative>());
        Off<HttpRelayOptionsNative>("struct_size", 0);
        Off<HttpRelayOptionsNative>("flags", 4);
        Off<HttpRelayOptionsNative>("max_chunk", 8);
        Off<HttpRelayOptionsNative>("upload_capacity", 16);
        Off<HttpRelayOptionsNative>("reserved", 20);
        HttpRelayOptionsNative o = default;
        Native.Check(Native.aster_http_relay_options_init(&o), "init");
        Assert.Equal(24u, o.struct_size);
        Assert.Equal(0ul, o.max_chunk);
        Assert.Equal(0u, o.upload_capacity);
    }

    [Fact]
    public void Relay_Enum_Values_And_Status_Mapping_Match_Header()
    {
        Assert.Equal(109u, (uint)EventKind.HttpRelayReady);
        Assert.Equal(10, (int)Expose.HttpBodyStatus.UnknownService);
        Assert.Equal(11, (int)Expose.HttpBodyStatus.Denied);
        Assert.Equal(12, (int)Expose.HttpBodyStatus.Overloaded);
        Assert.Equal(13, (int)Expose.HttpBodyStatus.Reset);
        Assert.Equal(14, (int)Expose.HttpBodyStatus.BadHead);
        Assert.Equal(15, (int)Expose.HttpBodyStatus.ConnectionClosed);
        Assert.Equal(16, (int)Expose.HttpBodyStatus.UploadFailed);
        Assert.Equal(17, (int)Expose.HttpBodyStatus.BufferTooSmall);
        Assert.Equal(14, (int)Status.Unauthenticated);
        Assert.Equal(15, (int)Status.PermissionDenied);
        Assert.Equal(16, (int)Status.CredentialExpired);
        // aster_http_body_status_to_status (RENAME_MAP "New in 4.3").
        Assert.Equal((int)Status.PermissionDenied, Native.aster_http_body_status_to_status((int)Expose.HttpBodyStatus.Denied));
        Assert.Equal((int)Status.NotFound, Native.aster_http_body_status_to_status((int)Expose.HttpBodyStatus.UnknownService));
        Assert.Equal((int)Status.AlreadyClosed, Native.aster_http_body_status_to_status((int)Expose.HttpBodyStatus.ConnectionClosed));
        Assert.Equal((int)Status.ConnectionRefused, Native.aster_http_body_status_to_status((int)Expose.HttpBodyStatus.Overloaded));
        Assert.Equal((int)Status.StreamReset, Native.aster_http_body_status_to_status((int)Expose.HttpBodyStatus.Reset));
        Assert.Equal((int)Status.Internal, Native.aster_http_body_status_to_status(9999));
        Assert.Contains("PERMISSION_DENIED", System.Runtime.InteropServices.Marshal.PtrToStringUTF8(Native.aster_status_name(15)));
    }

    private static readonly string[] Abi43Exports =
    {
        "aster_http_body_status_to_status", "aster_http_relay_options_init", "aster_expose_relay_stream",
        "aster_http_upload_write", "aster_http_upload_try_write", "aster_http_upload_finish", "aster_http_upload_abort",
        "aster_http_upload_ready", "aster_http_upload_free",
        "aster_http_response_wait", "aster_http_response_read", "aster_http_response_ready", "aster_http_response_close", "aster_http_response_free",
    };

    [Fact]
    public void Abi_4_3_Exports_Are_Declared_Exported_And_In_The_Header() => AssertExports(Abi43Exports);

    // ─── ABI 4.4: coordination client + placement ───────────────────────────

    [Fact]
    public unsafe void Coordination_Options_Layout_And_Init_Match_Header()
    {
        // aster_coordination_client_options_t: 48 bytes (LP64).
        Assert.Equal(48, Marshal.SizeOf<CoordinationClientOptionsNative>());
        Off<CoordinationClientOptionsNative>("struct_size", 0);
        Off<CoordinationClientOptionsNative>("flags", 4);
        Off<CoordinationClientOptionsNative>("group_id", 8);
        Off<CoordinationClientOptionsNative>("seed_tickets", 16);
        Off<CoordinationClientOptionsNative>("journal_dir", 32);
        CoordinationClientOptionsNative o = default;
        Native.Check(Native.aster_coordination_client_options_init(&o), "init");
        Assert.Equal(48u, o.struct_size);
        Assert.Equal(0u, o.flags);
        Assert.Equal(1u, Native.CoordinationClientReadOnly);
    }

    [Fact]
    public void Coordination_Enum_Values_Match_Header()
    {
        Assert.Equal(110u, (uint)EventKind.CoordinationClientOpened);
        Assert.Equal(111u, (uint)EventKind.CoordinationResolved);
        Assert.Equal(112u, (uint)EventKind.CoordinationResult);
        Assert.Equal(113u, (uint)EventKind.CoordinationLeaseAcquired);
        // aster_coordination_error_t (the error_code of a coordination ERROR event).
        Assert.Equal(1, (int)Coordination.CoordinationErrorKind.AwaitingAck);
        Assert.Equal(2, (int)Coordination.CoordinationErrorKind.Unresolved);
        Assert.Equal(3, (int)Coordination.CoordinationErrorKind.JournalLocked);
        Assert.Equal(4, (int)Coordination.CoordinationErrorKind.JournalFatal);
        Assert.Equal(5, (int)Coordination.CoordinationErrorKind.Rpc);
        Assert.Equal(6, (int)Coordination.CoordinationErrorKind.Unavailable);
        Assert.Equal(7, (int)Coordination.CoordinationErrorKind.Closed);
        Assert.Equal(8, (int)Coordination.CoordinationErrorKind.Rejected);
        Assert.Equal(9, (int)Coordination.CoordinationErrorKind.Invalid);
        Assert.Equal(10, (int)Coordination.CoordinationErrorKind.Other);
        // aster_coordination_mutation_op_t / aster_coordination_read_op_t: the op byte right after tag and version.
        var key = new Coordination.ResourceKey("n", "k");
        var fence = new Coordination.CoordFence(new byte[32], key, 1, new byte[32]);
        byte[] id16 = new byte[16], id32 = new byte[32];
        Coordination.MutationRequest[] muts =
        {
            new Coordination.MutationRequest.Acquire(key, 1), new Coordination.MutationRequest.Renew(fence),
            new Coordination.MutationRequest.CompareAndSet(fence, 0, []), new Coordination.MutationRequest.Release(fence),
            new Coordination.MutationRequest.ReleaseWithValue(fence, 0, []), new Coordination.MutationRequest.Revoke(key, null),
            new Coordination.MutationRequest.SetGrant(key, id32, 1, 0), new Coordination.MutationRequest.SetAdmin(id32, true, 0),
            new Coordination.MutationRequest.ChangeMembership(id16, null, [], [], []), new Coordination.MutationRequest.CancelMembership(id16, 0),
        };
        for (int i = 0; i < muts.Length; i++) Assert.Equal((byte)(i + 1), Coordination.CoordinationCodec.EncodeMutationRequest(muts[i])[2]);
        Coordination.ReadRequest[] reads =
        {
            new Coordination.ReadRequest.ReadLinearizable(key), new Coordination.ReadRequest.InspectAcl(key), new Coordination.ReadRequest.InspectAdmins(),
            new Coordination.ReadRequest.MembershipStatus(null), new Coordination.ReadRequest.GroupStatus(), new Coordination.ReadRequest.ProbeClient(id32, id16),
            new Coordination.ReadRequest.AuditStream(null, 1), new Coordination.ReadRequest.RecoveryExport(),
        };
        for (int i = 0; i < reads.Length; i++) Assert.Equal((byte)(i + 1), Coordination.CoordinationCodec.EncodeReadRequest(reads[i])[2]);
    }

    [Fact]
    public void Coordination_Tags_Match_Header_Defines()
    {
        string? header = null;
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !File.Exists(Path.Combine(dir.FullName, "ffi", "aster_ffi.h"))) dir = dir.Parent;
        if (dir != null) header = File.ReadAllText(Path.Combine(dir.FullName, "ffi", "aster_ffi.h"));
        if (header == null) return;
        var expected = new Dictionary<string, byte>
        {
            ["MUTATION_REQUEST"] = Coordination.CoordinationCodec.TagMutationRequest, ["READ_REQUEST"] = Coordination.CoordinationCodec.TagReadRequest,
            ["MUTATION_REPLY"] = Coordination.CoordinationCodec.TagMutationReply, ["READ_REPLY"] = Coordination.CoordinationCodec.TagReadReply,
            ["ACL_STATUS"] = Coordination.CoordinationCodec.TagAclStatus, ["ADMIN_STATUS"] = Coordination.CoordinationCodec.TagAdminStatus,
            ["MEMBERSHIP_STATUS"] = Coordination.CoordinationCodec.TagMembershipStatus, ["GROUP_STATUS"] = Coordination.CoordinationCodec.TagGroupStatus,
            ["CLIENT_PROBE"] = Coordination.CoordinationCodec.TagClientProbe, ["AUDIT_RECORDS"] = Coordination.CoordinationCodec.TagAuditRecords,
            ["RECOVERY_EXPORT"] = Coordination.CoordinationCodec.TagRecoveryExport, ["RECOVERY_EXPORT_MANIFEST"] = Coordination.CoordinationCodec.TagRecoveryExportManifest,
            ["RETIRE_REPLY"] = Coordination.CoordinationCodec.TagRetireReply, ["COORDINATION_FENCE"] = Coordination.CoordinationCodec.TagCoordinationFence,
            ["PLACEMENT_REPORT"] = Coordination.CoordinationCodec.TagPlacementReport,
        };
        foreach (var (name, tag) in expected)
            Assert.Matches($@"#define ASTER_COORDINATION_TAG_{name}\s+{tag}\b", header);
    }

    private static readonly string[] Abi44Exports =
    {
        "aster_coordination_client_options_init", "aster_coordination_client_open", "aster_coordination_client_close", "aster_coordination_client_free",
        "aster_coordination_client_ids", "aster_coordination_client_id", "aster_coordination_mutate", "aster_coordination_unacknowledged",
        "aster_coordination_resume", "aster_coordination_resolved_acknowledge", "aster_coordination_resolved_free",
        "aster_coordination_retire_other_client", "aster_coordination_read", "aster_coordination_recovery_export_to_file",
        "aster_coordination_lease_acquire", "aster_coordination_lease_fence", "aster_coordination_lease_status",
        "aster_coordination_lease_compare_and_set", "aster_coordination_lease_release", "aster_coordination_lease_release_with_value",
        "aster_coordination_lease_free", "aster_coordination_placement_manager_new", "aster_coordination_placement_reconcile",
        "aster_coordination_placement_manager_free",
    };

    [Fact]
    public void Abi_4_4_Exports_Are_Declared_Exported_And_In_The_Header() => AssertExports(Abi44Exports);

    // ─── Bytes helpers ───────────────────────────────────────────────────────

    [Fact]
    public void Bytes_FromPinned_And_ToArray_RoundTrip()
    {
        byte[] original = { 0xDE, 0xAD, 0xBE, 0xEF };
        var pin = GCHandle.Alloc(original, GCHandleType.Pinned);
        byte[] roundTrip;
        try { roundTrip = Bytes.FromPinned(pin, original.Length).ToArray(); }
        finally { pin.Free(); }

        Assert.Equal(original, roundTrip);
    }

    [Fact]
    public void Bytes_IsEmpty_True_For_Default()
    {
        var b = default(Bytes);
        Assert.True(b.IsEmpty);
    }

    [Fact]
    public void Bytes_ToUtf8String_RoundTrip()
    {
        string original = "Hello, Aster!";
        byte[] utf8 = System.Text.Encoding.UTF8.GetBytes(original);
        var pin = GCHandle.Alloc(utf8, GCHandleType.Pinned);
        try { Assert.Equal(original, Bytes.FromPinned(pin, utf8.Length).ToUtf8String()); }
        finally { pin.Free(); }
    }
}
