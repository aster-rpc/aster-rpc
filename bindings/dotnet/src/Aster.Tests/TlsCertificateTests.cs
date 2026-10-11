using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Security.Cryptography.X509Certificates;
using System.Text;
using Aster.Crypto;

namespace Aster.Tests;

public class TlsCertificateTests
{
    [Theory]
    [InlineData(false, false)]
    [InlineData(false, true)]
    [InlineData(true, false)]
    [InlineData(true, true)]
    public void NativeCertificateGenerationAndSignedBinding(bool webTransport, bool signed)
    {
        byte[] secret = Ids.GenerateSecretKey();
        string expected = Ids.NodeId(secret);
        byte[] nodeSecret = signed ? secret : [];
        var generated = webTransport
            ? TlsCertificates.GenerateWebTransportCertificate("display-only", ["127.0.0.1"], nodeSecret)
            : TlsCertificates.GenerateSelfSignedCertificate(["localhost", "127.0.0.1"], nodeSecret);
        using var cert = X509Certificate2.CreateFromPem(Encoding.ASCII.GetString(generated.CertificatePem), Encoding.ASCII.GetString(generated.PrivateKeyPem));
        Assert.True(cert.HasPrivateKey);
        Assert.Equal(SHA256.HashData(cert.RawData), generated.Sha256);
        using var key = cert.GetECDsaPublicKey();
        Assert.NotNull(key);
        Assert.Equal(256, key.KeySize);
        Assert.True(cert.MatchesHostname("localhost"));
        Assert.True(cert.MatchesHostname("127.0.0.1"));
        if (webTransport)
        {
            Assert.InRange(cert.NotAfter.ToUniversalTime() - cert.NotBefore.ToUniversalTime(), TimeSpan.Zero, TimeSpan.FromDays(14));
            Assert.InRange(DateTime.UtcNow, cert.NotBefore.ToUniversalTime(), cert.NotAfter.ToUniversalTime());
        }
        if (signed) TlsCertificates.VerifyCertificateBinding(generated.CertificatePem, expected);
        else Assert.Equal(Status.InvalidArgument, Assert.Throws<IrohException>(() => TlsCertificates.VerifyCertificateBinding(generated.CertificatePem, expected)).Status);
        Assert.Equal(Status.InvalidArgument, Assert.Throws<IrohException>(() => TlsCertificates.VerifyCertificateBinding(generated.CertificatePem, new string('0', 64))).Status);
    }

    [Fact]
    public void InvalidInputsAndDefaultSAN()
    {
        Assert.Throws<ArgumentException>(() => TlsCertificates.GenerateSelfSignedCertificate(nodeSecret: new byte[1]));
        Assert.Throws<ArgumentException>(() => TlsCertificates.GenerateWebTransportCertificate("subject", nodeSecret: new byte[33]));
        Assert.Throws<ArgumentException>(() => TlsCertificates.VerifyCertificateBinding("bad PEM"u8, ""));
        Assert.Equal(Status.InvalidArgument, Assert.Throws<IrohException>(() => TlsCertificates.VerifyCertificateBinding("bad PEM"u8, new string('0', 64))).Status);
        var generated = TlsCertificates.GenerateSelfSignedCertificate();
        using var cert = X509Certificate2.CreateFromPem(Encoding.ASCII.GetString(generated.CertificatePem));
        Assert.True(cert.MatchesHostname("localhost"));
    }

    [Fact]
    public void GeneratedCertificateAbiLayout()
    {
        Assert.Equal(64, Marshal.SizeOf<GeneratedCertNative>());
        Assert.Equal(0, Marshal.OffsetOf<GeneratedCertNative>("cert_pem").ToInt32());
        Assert.Equal(16, Marshal.OffsetOf<GeneratedCertNative>("key_pem").ToInt32());
        Assert.Equal(32, Marshal.OffsetOf<GeneratedCertNative>("sha256").ToInt32());
    }
}
