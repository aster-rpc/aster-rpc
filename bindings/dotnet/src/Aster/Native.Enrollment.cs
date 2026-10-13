using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for C ABI 5.1: Enrollment from configuration and the lease grantor
// (ffi/RENAME_MAP.md "New in 5.1"; design docs/_internal/working_ideas/aster-enrollment.md §15.10).
internal static unsafe partial class Native
{
    /// <summary><c>aster_enrollment_side_t</c>: a server.</summary>
    public const uint EnrollmentSideServer = 0;
    /// <summary><c>aster_enrollment_side_t</c>: a client.</summary>
    public const uint EnrollmentSideClient = 1;

    [LibraryImport(NativeLib, EntryPoint = "aster_config_enrollment_from_env")]
    public static partial int aster_config_enrollment_from_env(Bytes* out_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_config_enrollment_from_toml")]
    public static partial int aster_config_enrollment_from_toml(Bytes path, Bytes* out_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_node_start_enrolled")]
    public static partial int aster_node_start_enrolled(ulong runtime, ConfigNative* config, Bytes trust_json, uint side,
        ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_enroll")]
    public static partial int aster_enrollment_enroll(ulong runtime, ulong enrollment, Bytes server_json, ulong user_data,
        out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_free")]
    public static partial int aster_enrollment_free(ulong runtime, ulong enrollment);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_check_peer")]
    public static partial int aster_enrollment_check_peer(ulong runtime, ulong enrollment, Bytes peer, ulong user_data,
        out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_peer_claims")]
    public static partial int aster_enrollment_peer_claims(ulong runtime, ulong enrollment, Bytes peer, Bytes* out_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_status")]
    public static partial int aster_enrollment_status(ulong runtime, ulong enrollment, Bytes* out_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_next_revocation")]
    public static partial int aster_enrollment_next_revocation(ulong runtime, ulong enrollment, ulong user_data,
        out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_issuer_info")]
    public static partial int aster_enrollment_issuer_info(ulong runtime, ulong enrollment, Bytes* out_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_issue")]
    public static partial int aster_enrollment_issue(ulong runtime, ulong enrollment, Bytes request_json, Bytes* out_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_server_add_lease_grantor")]
    public static partial int aster_server_add_lease_grantor(ulong builder, ulong serializer, Bytes requires_json,
        Bytes revoke_requires_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_lease_serializer_grantor_new")]
    public static partial int aster_lease_serializer_grantor_new(ulong runtime, ulong node, ulong connection,
        out ulong out_serializer);
}
