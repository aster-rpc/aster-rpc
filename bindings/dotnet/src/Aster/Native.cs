using System;
using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// Low-level FFI declarations for the iroh C ABI, generated via source-generated P/Invoke.
/// All native symbols are resolved from the native library at load time.
/// </summary>
internal static partial class Native
{
    private const string NativeLib = "libaster_transport_ffi";

    // ─── Version ────────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_abi_version_major")]
    public static partial int iroh_abi_version_major();

    [LibraryImport(NativeLib, EntryPoint = "iroh_abi_version_minor")]
    public static partial int iroh_abi_version_minor();

    [LibraryImport(NativeLib, EntryPoint = "iroh_abi_version_patch")]
    public static partial int iroh_abi_version_patch();

    // ─── Status ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_status_name")]
    public static partial IntPtr iroh_status_name(uint code);

    [LibraryImport(NativeLib, EntryPoint = "iroh_last_error_message", StringMarshalling = StringMarshalling.Utf8)]
    public static partial int iroh_last_error_message(byte[] buf, int buf_len);

    // ─── Runtime ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_runtime_new")]
    public static partial int iroh_runtime_new(ref RuntimeConfig config, out ulong out_runtime);

    [LibraryImport(NativeLib, EntryPoint = "iroh_runtime_close")]
    public static partial int iroh_runtime_close(ulong runtime);

    [LibraryImport(NativeLib, EntryPoint = "iroh_poll_events")]
    public static partial int iroh_poll_events(ulong runtime, IntPtr events, int max_events, int timeout_ms);

    // ─── Buffers ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_buffer_release")]
    public static partial int iroh_buffer_release(ulong runtime, ulong buffer);

    // ─── Strings ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_string_release")]
    public static partial int iroh_string_release(IntPtr data, UIntPtr len);

    // ─── Operation cancellation ───────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_operation_cancel")]
    public static partial int iroh_operation_cancel(ulong runtime, ulong operation);
}
