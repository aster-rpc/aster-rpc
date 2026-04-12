using System;
using System.Text;

namespace Aster;

/// <summary>
/// Computes Aster contract identities via the Rust FFI canonicalizer.
/// All canonicalization and BLAKE3 hashing is done in Rust -- .NET never computes
/// canonical bytes or hashes locally. Per spec 11.3.2.3.
/// </summary>
public static class ContractIdentity
{
    private const int ContractIdBufSize = 128;

    /// <summary>
    /// Compute the contract_id (64-char hex BLAKE3) from a ServiceContract JSON string.
    /// The JSON must match the serde shape of core::contract::ServiceContract.
    /// </summary>
    public static unsafe string ComputeContractId(string serviceContractJson)
    {
        byte[] jsonBytes = Encoding.UTF8.GetBytes(serviceContractJson);
        byte[] outBuf = new byte[ContractIdBufSize];

        fixed (byte* jsonPtr = jsonBytes)
        fixed (byte* outPtr = outBuf)
        {
            UIntPtr outLen = (UIntPtr)ContractIdBufSize;
            int status = Native.aster_contract_id(jsonPtr, (UIntPtr)jsonBytes.Length, outPtr, &outLen);
            if (status != 0)
                throw new InvalidOperationException(
                    $"aster_contract_id failed with status {status}. Check the ServiceContract JSON.");

            return Encoding.UTF8.GetString(outBuf, 0, (int)(ulong)outLen);
        }
    }

    /// <summary>
    /// Compute canonical bytes for a named type from JSON.
    /// typeName: "ServiceContract", "TypeDef", or "MethodDef".
    /// </summary>
    public static unsafe byte[] ComputeCanonicalBytes(string typeName, string json)
    {
        byte[] typeNameBytes = Encoding.UTF8.GetBytes(typeName);
        byte[] jsonBytes = Encoding.UTF8.GetBytes(json);
        int bufSize = 4096;
        byte[] outBuf = new byte[bufSize];

        fixed (byte* typeNamePtr = typeNameBytes)
        fixed (byte* jsonPtr = jsonBytes)
        fixed (byte* outPtr = outBuf)
        {
            UIntPtr outLen = (UIntPtr)bufSize;
            int status = Native.aster_canonical_bytes(
                typeNamePtr, (UIntPtr)typeNameBytes.Length,
                jsonPtr, (UIntPtr)jsonBytes.Length,
                outPtr, &outLen);

            if (status != 0)
                throw new InvalidOperationException(
                    $"aster_canonical_bytes failed with status {status}.");

            int written = (int)(ulong)outLen;
            byte[] result = new byte[written];
            Array.Copy(outBuf, result, written);
            return result;
        }
    }
}
