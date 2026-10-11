using System.Runtime.InteropServices;
namespace Aster;

[StructLayout(LayoutKind.Sequential)]
internal struct ProvisioningResultNative { public Bytes data; public Bytes error; }

internal static partial class Native
{
    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_canonical_genesis(Bytes json, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_canonical_join_permit(Bytes json, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_genesis_signing_bytes(Bytes document, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_join_signing_bytes(Bytes document, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_genesis_hash(Bytes document, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_signed_document_from_parts(Bytes document, Bytes authorityKey, Bytes signature, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_canonical_candidate_ticket(Bytes node, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_verify_genesis(Bytes authorityKey, Bytes domain, Bytes envelope, out ProvisioningResultNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_coordination_verify_join_permit_for_start(Bytes authorityKey, Bytes domain, Bytes candidate, Bytes envelope, out ProvisioningResultNative output);

}
