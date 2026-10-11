using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// Copied PEM material and the SHA-256 of the complete DER certificate, suitable
/// for WebTransport serverCertificateHashes. Keep PrivateKeyPem secret.
/// Generation alone does not establish external trust.
/// </summary>
public sealed record TlsCertificate(byte[] CertificatePem, byte[] PrivateKeyPem, byte[] Sha256);

/// <summary>Native ECDSA certificate generation and signed Aster identity binding verification (ABI 4.12).</summary>
public static unsafe class TlsCertificates
{
    /// <summary>
    /// Generate a self-signed ECDSA certificate. Empty SANs select localhost.
    /// An empty nodeSecret leaves it unbound; exactly 32 bytes adds a signed
    /// Aster identity binding. No unsigned identity claim is emitted.
    /// </summary>
    public static TlsCertificate GenerateSelfSignedCertificate(IReadOnlyList<string>? sans = null,
        ReadOnlySpan<byte> nodeSecret = default) => Generate("", sans, nodeSecret, false);

    /// <summary>
    /// Generate a WebTransport-suitable ECDSA P-256 certificate with validity
    /// at most 14 days, including localhost and the supplied SANs. Subject is a
    /// display name, not an identity proof. A 32-byte nodeSecret signs its Aster
    /// identity binding; an empty secret leaves it unbound.
    /// </summary>
    public static TlsCertificate GenerateWebTransportCertificate(string subject, IReadOnlyList<string>? sans = null,
        ReadOnlySpan<byte> nodeSecret = default)
    {
        ArgumentNullException.ThrowIfNull(subject);
        return Generate(subject, sans, nodeSecret, true);
    }

    private static TlsCertificate Generate(string subject, IReadOnlyList<string>? sans, ReadOnlySpan<byte> nodeSecret, bool webTransport)
    {
        if (!nodeSecret.IsEmpty && nodeSecret.Length != 32)
            throw new ArgumentException("node secret must be empty or exactly 32 bytes", nameof(nodeSecret));
        using var scope = new NativeScope();
        var names = scope.Utf8List(sans ?? Array.Empty<string>());
        var secret = scope.Bytes(nodeSecret);
        GeneratedCertNative output = default;
        try
        {
            if (webTransport)
                Native.Check(Native.aster_tls_generate_webtransport(scope.Utf8(subject), names, secret, out output), "aster_tls_generate_webtransport");
            else
                Native.Check(Native.aster_tls_generate_self_signed(names, secret, out output), "aster_tls_generate_self_signed");
            return new TlsCertificate(NativeScope.Read(output.cert_pem), NativeScope.Read(output.key_pem),
                new ReadOnlySpan<byte>(output.sha256, 32).ToArray());
        }
        finally
        {
            if (output.cert_pem.ptr != IntPtr.Zero) Native.aster_string_release(output.cert_pem.ptr, output.cert_pem.len);
            if (output.key_pem.ptr != IntPtr.Zero) Native.aster_string_release(output.key_pem.ptr, output.key_pem.len);
        }
    }

    /// <summary>
    /// Verify a signed TLS public-key binding against a node ID learned through
    /// a trusted channel. Never trust an ID taken only from the certificate.
    /// This does not verify the whole certificate, TLS chain, hostname, validity
    /// period or trust in the expected node ID.
    /// </summary>
    public static void VerifyCertificateBinding(ReadOnlySpan<byte> certificatePem, string trustedExpectedNodeId)
    {
        ArgumentException.ThrowIfNullOrEmpty(trustedExpectedNodeId);
        using var scope = new NativeScope();
        Native.Check(Native.aster_tls_verify_binding(scope.Bytes(certificatePem), scope.Utf8(trustedExpectedNodeId)), "aster_tls_verify_binding");
    }
}

[StructLayout(LayoutKind.Sequential)]
internal unsafe struct GeneratedCertNative
{
    public Bytes cert_pem;
    public Bytes key_pem;
    public fixed byte sha256[32];
}

internal static unsafe partial class Native
{
    [LibraryImport(NativeLib)]
    internal static partial int aster_tls_generate_self_signed(BytesList sans, Bytes nodeSecret, out GeneratedCertNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_tls_generate_webtransport(Bytes subject, BytesList sans, Bytes nodeSecret, out GeneratedCertNative output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_tls_verify_binding(Bytes certificatePem, Bytes expectedNodeId);
}
