//! Certificate creation and signed node binding; trust and hostname checks remain with TLS clients.
use aster_transport_salvo::NodeBinding;
use napi::bindgen_prelude::*;
use napi_derive::napi;

#[napi(object)]
pub struct GeneratedCertificate {
    pub certificate_pem: Buffer,
    pub private_key_pem: Buffer,
    pub sha256: Buffer,
}

fn output(cert: aster_transport_salvo::GeneratedCert) -> GeneratedCertificate {
    GeneratedCertificate {
        certificate_pem: cert.cert_pem.into(),
        private_key_pem: cert.key_pem.into(),
        sha256: cert.sha256.to_vec().into(),
    }
}

#[napi]
pub fn generate_self_signed_certificate(
    sans: Vec<String>,
    node_secret: Option<Buffer>,
) -> Result<GeneratedCertificate> {
    let secret = node_secret
        .as_ref()
        .map(|v| crate::keys::bytes32("node secret", v))
        .transpose()?;
    aster_transport_salvo::generate_self_signed(secret.as_ref().map(NodeBinding::Signed), &sans)
        .map(output)
        .map_err(Error::from_reason)
}

#[napi(js_name = "generateWebTransportCertificate")]
pub fn generate_webtransport_certificate(
    subject: String,
    sans: Vec<String>,
    node_secret: Option<Buffer>,
) -> Result<GeneratedCertificate> {
    let secret = node_secret
        .as_ref()
        .map(|v| crate::keys::bytes32("node secret", v))
        .transpose()?;
    aster_transport_salvo::generate_webtransport_cert(
        &subject,
        secret.as_ref().map(NodeBinding::Signed),
        &sans,
    )
    .map(output)
    .map_err(Error::from_reason)
}

#[napi]
pub fn verify_certificate_binding(certificate_pem: Buffer, expected_node_id: String) -> Result<()> {
    aster_transport_salvo::verify_cert_binding(&certificate_pem, &expected_node_id)
        .map_err(Error::from_reason)
}
