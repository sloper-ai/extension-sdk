mod error;

use std::{
    collections::BTreeMap,
    fmt,
    path::Path,
    time::Duration,
};

use base64::{
    Engine as _,
    engine::general_purpose::URL_SAFE_NO_PAD,
};
use generated_client::{
    CreateExtensionVersionRequest,
    ExtensionVisibility,
    GetReleaseEnvelopeRequest,
    Problem,
    PublishExtensionVersionRequest,
};
pub use generated_client::{
    ExtensionComponentDownload,
    ExtensionVersion,
    ExtensionVersionState,
    OwnerKind,
    PublisherIdentity,
    ReleaseEnvelope,
};
use serde::{
    Deserialize,
    Serialize,
};
use sha2::{
    Digest as _,
    Sha256,
};
use sloper_extension_host::{
    Engine,
    Trust,
    TrustRoots,
    extract_manifest,
    validate_component,
};
use sloper_extension_spec::parse_object;
use time::{
    OffsetDateTime,
    format_description::well_known::Rfc3339,
};
use zeroize::Zeroizing;

use self::error::Error as ApiError;
use crate::{
    Error,
    hex,
    read_bounded,
    read_component,
};

/// Default Sloper extension publication API endpoint.
pub const DEFAULT_API_URL: &str = "https://console.sloper.ai";
const MAX_TOKEN_BYTES: usize = 16_384;

/// Audience selected for an extension's first publication.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Available only to the publisher's owner.
    Private,
    /// Available to authenticated Sloper users; restricted to Sloper
    /// publishers.
    Public,
}
impl Visibility {
    /// Returns the publication API spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

/// Explicit publication inputs. Tokens never cross a Cargo or guest boundary.
pub struct PublishOptions<'a> {
    /// Directory with existing `dist/extension.wasm` and optional
    /// `dist/icon.png`.
    pub directory: &'a Path,
    /// Trusted API endpoint selected by the caller.
    pub api_url: &'a str,
    /// Publishing bearer; use a scoped token for CI.
    pub token: &'a str,
    /// Omit to inherit an existing audience or the service's private default.
    pub visibility: Option<Visibility>,
}
impl fmt::Debug for PublishOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublishOptions")
            .field("visibility", &self.visibility)
            .finish_non_exhaustive()
    }
}

/// Published immutable version and matching, byte-checked release envelope.
#[derive(Debug, Serialize)]
pub struct PublishResult {
    /// Newly published or idempotently replayed version.
    pub version: ExtensionVersion,
    /// Envelope whose identity and descriptors match the exact submitted bytes.
    pub envelope: ReleaseEnvelope,
}

/// An authenticated publication request or receipt lookup failed.
///
/// The generated HTTP client's representation is kept private. Error sources
/// remain available for diagnostics; command output exposes only declared
/// problem details and stable exit categories.
#[derive(thiserror::Error)]
#[error("The publication request failed")]
pub struct PublicationError(#[source] Box<ApiError>);

impl fmt::Debug for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PublicationError")
    }
}

impl PublicationError {
    pub(crate) fn exit_code(&self) -> u8 {
        api_exit(&self.0)
    }

    pub(crate) fn details(&self) -> serde_json::Value {
        api_details(&self.0)
    }
}

fn api_details(error: &ApiError) -> serde_json::Value {
    let Some(error) = error.api() else {
        return serde_json::json!({});
    };
    let Some(status) = error.status else {
        return serde_json::json!({});
    };
    let mut result = serde_json::json!({"status":status.as_u16()});
    // Only declared problem findings cross this boundary; arbitrary response
    // bodies and bearer-bearing transport diagnostics stay private.
    if let Ok(problem) = serde_json::from_slice::<Problem>(&error.raw_body) {
        result["code"] = serde_json::json!(problem.code);
        if let Some(errors) = problem.errors.filter(|errors| !errors.is_empty()) {
            result["errors"] = serde_json::json!(errors);
        }
    }
    result
}

fn api_exit(error: &ApiError) -> u8 {
    if error
        .api()
        .is_some_and(|error| error.kind == generated_client::ErrorKind::Request)
    {
        return 2;
    }
    match error.api().and_then(|error| error.status).map(|status| status.as_u16()) {
        Some(401) => 6,
        Some(403) => 8,
        Some(404) => 5,
        Some(409 | 412 | 428) => 4,
        Some(400 | 413 | 415 | 422) => 3,
        Some(429 | 502 | 503 | 504) | None => 9,
        Some(_) => 10,
    }
}

impl From<ApiError> for Error {
    fn from(error: ApiError) -> Self {
        PublicationError(Box::new(error)).into()
    }
}

/// Existing artifacts and explicitly trusted roots for release verification.
#[derive(Debug)]
pub struct VerifyOptions<'a> {
    /// Directory containing `dist/extension.wasm`.
    pub directory: &'a Path,
    /// Exact signed release line.
    pub envelope: &'a Path,
    /// Signed trust document.
    pub trust: &'a Path,
    /// Roots supplied out of band by the application or developer.
    pub roots: &'a TrustRoots,
}

/// Authenticated release admitted by the local runtime.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    /// Extension identity.
    pub extension_id: String,
    /// Exact version.
    pub version: String,
    /// Publisher identity authenticated by the envelope.
    pub publisher: String,
    /// Digest of the exact signed envelope line.
    pub release_envelope_id: String,
    /// Release signing key identity.
    pub key: String,
    /// Exact embedded manifest digest.
    pub manifest_digest: String,
    /// Exact component digest.
    pub component_digest: String,
    /// RFC 3339 expiration of the authenticated trust document.
    pub trust_expire_time: String,
    /// False after the mandatory freshness check.
    pub trust_stale: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptPayload {
    extension: String,
    version: String,
    publisher: String,
    manifest: ObjectIdentity,
    component: ObjectIdentity,
    key: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectIdentity {
    digest: String,
    bytes: u64,
}

/// Publishes exact existing distributable bytes, then checks the returned
/// receipt.
///
/// Static validation does not instantiate guest code. The receipt check
/// verifies HTTPS response consistency; cryptographic release verification uses
/// [`verify`] with independently provisioned roots.
///
/// # Errors
/// Returns static validation, credential, transport, API, or receipt mismatch
/// errors.
///
/// # Cancel safety
/// Cancellation after sending may leave a published version. Repeating with the
/// same bytes and audience uses the same idempotency key and safely recovers
/// it.
pub async fn publish(options: PublishOptions<'_>) -> Result<PublishResult, Error> {
    if options.token.is_empty() || options.token.len() > MAX_TOKEN_BYTES || options.token.chars().any(char::is_control)
    {
        return Err(Error::usage("publishing token must be a nonempty, bounded HTTP bearer"));
    }
    let origin = api_origin(options.api_url)?;
    let component = read_component(&options.directory.join("dist/extension.wasm"))?;
    let manifest = validate_component(&component)?;
    let manifest_bytes = extract_manifest(&component)?;
    let component_digest = digest(&component);
    let manifest_digest = digest(manifest_bytes);
    let manifest_bytes = manifest_bytes.to_vec();
    let component_length = component.len() as u64;
    let icon_path = options.directory.join("dist/icon.png");
    let icon = if icon_path.try_exists()? {
        Some(crate::icon::read(&icon_path)?)
    } else {
        None
    };
    let mut identity = Sha256::new();
    identity.update(&component);
    if let Some(icon) = &icon {
        identity.update(icon);
    }
    // Audience is an authorized part of the request, so changes cannot replay
    // an earlier publication before the service evaluates that audience.
    if let Some(visibility) = options.visibility {
        identity.update(visibility.as_str());
    }
    let request_id = format!("sha256:{}", hex(&identity.finalize()));
    let body = PublishExtensionVersionRequest {
        component,
        icon,
        visibility: options.visibility.map(|visibility| {
            match visibility {
                Visibility::Private => ExtensionVisibility::Private,
                Visibility::Public => ExtensionVisibility::Public,
            }
        }),
        additional_properties: BTreeMap::new(),
    };
    let token = Zeroizing::new(options.token.to_owned());
    let mut config = generated_client::Config::new()
        .with_api_base(format!("{}/api/v1", options.api_url.trim_end_matches('/')))
        .with_timeout(Duration::from_secs(120))
        .with_api_key(token);
    config.headers.insert("origin", origin.parse().map_err(ApiError::from)?);
    let transport = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let api = generated_client::Client::with_http_client(config, transport);
    let response = api
        .resources()
        .extensions()
        .create_version(CreateExtensionVersionRequest {
            extension: manifest.name.clone(),
            idempotency_key: request_id,
            sloper_version: None,
            body,
        })
        .await?;
    let version = response
        .body
        .ok_or_else(|| Error::invalid_response("The publication service returned no extension version"))?;
    if version.extension_id != manifest.name || version.version != manifest.version {
        return Err(Error::invalid_response(
            "The published identity differs from the uploaded component",
        ));
    }
    let response = api
        .resources()
        .extensions()
        .get_release_envelope(GetReleaseEnvelopeRequest {
            extension: manifest.name.clone(),
            release_envelope: version.release_envelope_id.clone(),
            if_none_match: None,
            sloper_version: None,
        })
        .await?;
    let envelope = response
        .body
        .ok_or_else(|| Error::invalid_response("The publication service returned no release envelope"))?;
    verify_receipt(
        &version,
        &envelope,
        &component_digest,
        component_length,
        &manifest_digest,
        &manifest_bytes,
    )?;
    Ok(PublishResult {
        version,
        envelope,
    })
}

fn api_origin(api_url: &str) -> Result<String, Error> {
    let invalid = || Error::usage("API URL must be an absolute HTTP or HTTPS URL");
    let url = url::Url::parse(api_url).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return Err(invalid());
    }
    Ok(url.origin().ascii_serialization())
}

fn verify_receipt(
    version: &ExtensionVersion,
    envelope: &ReleaseEnvelope,
    component_digest: &str,
    component_length: u64,
    manifest_digest: &str,
    manifest_bytes: &[u8],
) -> Result<(), Error> {
    let invalid = || Error::invalid_response("The release receipt does not match the exact submitted bytes");
    if envelope.envelope.is_empty() || envelope.envelope.len() > 16 * 1024 || !envelope.envelope.is_ascii() {
        return Err(invalid());
    }
    let (payload, signature) = envelope.envelope.split_once('.').ok_or_else(invalid)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid())?;
    let payload: ReceiptPayload =
        serde_json::from_value(parse_object(&payload).map_err(|_| invalid())?).map_err(|_| invalid())?;
    let publisher = &envelope.publisher.name;
    if signature.len() != 64
        || payload.key.is_empty()
        || payload.publisher != *publisher
        || payload.extension != version.extension_id
        || payload.version != version.version
        || envelope.extension_id != version.extension_id
        || envelope.version != version.version
        || envelope.id != version.release_envelope_id
        || digest(envelope.envelope.as_bytes()) != envelope.id
        || payload.component.digest != component_digest
        || payload.component.bytes != component_length
        || envelope.component.sha256 != component_digest
        || u64::try_from(envelope.component.bytes).ok() != Some(component_length)
        || payload.manifest.digest != manifest_digest
        || payload.manifest.bytes != manifest_bytes.len() as u64
        || envelope.manifest.as_bytes() != manifest_bytes
    {
        return Err(invalid());
    }
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

/// Authenticates the trust document and exact release before runtime admission.
///
/// # Errors
/// Rejects invalid, stale, revoked, mismatched, or incompatible releases.
/// # Cancel safety
/// Cancellation does not modify trust or any application state.
pub async fn verify(options: VerifyOptions<'_>) -> Result<VerifyResult, Error> {
    let now = OffsetDateTime::now_utc();
    let trust = Trust::verify(&read_bounded(options.trust, 1024 * 1024)?, options.roots, now, None)?;
    trust.require_fresh(now)?;
    let envelope = read_bounded(options.envelope, 16 * 1024)?;
    let component = read_component(&options.directory.join("dist/extension.wasm"))?;
    let admitted = Engine::new()?.admit_release(&envelope, &component, &trust).await?;
    let release = admitted.release();
    Ok(VerifyResult {
        extension_id: release.extension().to_owned(),
        version: release.version().to_owned(),
        publisher: release.publisher().to_owned(),
        release_envelope_id: release.digest().to_owned(),
        key: release.key().to_owned(),
        manifest_digest: release.manifest_digest().to_owned(),
        component_digest: release.component_digest().to_owned(),
        trust_expire_time: trust
            .expires_at()
            .format(&Rfc3339)
            .map_err(|_| Error::invalid_response("trust expiry cannot be encoded"))?,
        trust_stale: trust.is_stale(now),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{
            Read as _,
            Write as _,
        },
        net::TcpListener,
        thread,
    };

    use generated_client::{
        ExtensionComponentDownload,
        ExtensionVersionState,
        OwnerKind,
        PublisherIdentity,
    };
    use serde_json::json;

    use super::*;

    const COMPONENT: &[u8] = include_bytes!("../tests/fixtures/host-guest.wasm");

    fn receipt(component: &[u8]) -> PublishResult {
        let manifest = validate_component(component).unwrap();
        let manifest_bytes = extract_manifest(component).unwrap();
        let payload = json!({"extension":manifest.name,"version":manifest.version.clone(),"publisher":"Sloper","key":"test-key","component":{"digest":digest(component),"bytes":component.len()},"manifest":{"digest":digest(manifest_bytes),"bytes":manifest_bytes.len()}});
        let line = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap()),
            URL_SAFE_NO_PAD.encode([0; 64])
        );
        let id = digest(line.as_bytes());
        PublishResult {
            version: ExtensionVersion {
                extension_id: manifest.name.clone(),
                version: manifest.version.clone(),
                release_envelope_id: id.clone(),
                published_at: "2026-09-07T00:00:00Z".into(),
                state: ExtensionVersionState::Published,
                reason: None,
                additional_properties: BTreeMap::new(),
            },
            envelope: ReleaseEnvelope {
                extension_id: manifest.name,
                version: manifest.version.clone(),
                id,
                envelope: line,
                manifest: String::from_utf8(manifest_bytes.to_vec()).unwrap(),
                publisher: PublisherIdentity {
                    id: "sloper".into(),
                    kind: OwnerKind::Organization,
                    handle: "sloper".into(),
                    name: "Sloper".into(),
                    additional_properties: BTreeMap::new(),
                },
                component: ExtensionComponentDownload {
                    bytes: i64::try_from(component.len()).unwrap(),
                    sha256: digest(component),
                    expires_at: "2026-09-07T00:05:00Z".into(),
                    url: "https://artifacts.example/extension.wasm".into(),
                    additional_properties: BTreeMap::new(),
                },
                additional_properties: BTreeMap::new(),
            },
        }
    }
    fn check(receipt: &PublishResult) -> Result<(), Error> {
        let manifest = extract_manifest(COMPONENT).unwrap();
        verify_receipt(
            &receipt.version,
            &receipt.envelope,
            &digest(COMPONENT),
            COMPONENT.len() as u64,
            &digest(manifest),
            manifest,
        )
    }
    fn serve(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            // Publication and evidence reads must reuse the same composed client
            // and connection pool. A per-operation factory cannot satisfy this peer.
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            for response in responses {
                let mut request = Vec::new();
                let mut chunk = [0; 8192];
                loop {
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0, "request terminated before its body");
                    request.extend_from_slice(&chunk[..count]);
                    if let Some(index) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..index]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                        if request.len() >= index + 4 + length {
                            break;
                        }
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSloper-Version: 2026-09-08\r\nX-Request-Id: \
                     publication-test\r\nRateLimit: available\r\nRateLimit-Policy: test\r\nETag: \
                     fixture\r\nCache-Control: private, no-cache\r\nContent-Length: {}\r\nConnection: \
                     keep-alive\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
                requests.push(request);
            }
            requests
        });
        (address, worker)
    }

    #[test]
    fn receipt_checks_exact_manifest_component_and_envelope_identity() {
        let mut result = receipt(COMPONENT);
        check(&result).unwrap();
        result.envelope.manifest.push(' ');
        assert!(check(&result).is_err());
        let mut result = receipt(COMPONENT);
        result.envelope.component.sha256 = digest(b"other");
        assert!(check(&result).is_err());
        let mut result = receipt(COMPONENT);
        result.envelope.envelope.push('x');
        assert!(check(&result).is_err());
    }

    #[tokio::test]
    async fn publication_reuses_connection_for_exact_bytes_and_receipt_with_explicit_bearer() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("dist")).unwrap();
        fs::write(directory.path().join("dist/extension.wasm"), COMPONENT).unwrap();
        let receipt = receipt(COMPONENT);
        let mut version_json = serde_json::to_value(&receipt.version).unwrap();
        version_json["reason"] = serde_json::Value::Null;
        let (origin, worker) = serve(vec![
            version_json.to_string(),
            serde_json::to_string(&receipt.envelope).unwrap(),
        ]);
        let api_url = format!("{origin}/delivery/");
        let result = publish(PublishOptions {
            directory: directory.path(),
            api_url: &api_url,
            token: "explicit-publishing-bearer",
            visibility: Some(Visibility::Public),
        })
        .await
        .unwrap_or_else(|error| panic!("publication failed: {}", error.command_error()));
        assert_eq!(result.version.release_envelope_id, receipt.version.release_envelope_id);
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].starts_with(
                format!(
                    "POST /delivery/api/v1/extensions/{}/versions HTTP/1.1\r\n",
                    receipt.version.extension_id
                )
                .as_bytes()
            )
        );
        assert!(
            requests[1].starts_with(
                format!(
                    "GET /delivery/api/v1/extensions/{}/release-envelopes/sha256%3A",
                    receipt.version.extension_id
                )
                .as_bytes()
            )
        );
        for request in &requests {
            let end = request.windows(4).position(|value| value == b"\r\n\r\n").unwrap();
            let headers = std::str::from_utf8(&request[..end]).unwrap();
            assert_eq!(
                headers.lines().find_map(|line| line.strip_prefix("origin: ")),
                Some(origin.as_str())
            );
        }
        assert!(requests[0].windows(COMPONENT.len()).any(|bytes| bytes == COMPONENT));
        let headers = String::from_utf8_lossy(
            &requests[0][..requests[0].windows(4).position(|value| value == b"\r\n\r\n").unwrap()],
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer explicit-publishing-bearer")
        );
        assert!(String::from_utf8_lossy(&requests[0]).contains("name=\"visibility\""));
        assert!(String::from_utf8_lossy(&requests[0]).contains("\r\n\r\npublic\r\n"));
        assert!(!directory.path().join("Cargo.toml").exists());
        assert_eq!(
            fs::read(directory.path().join("dist/extension.wasm")).unwrap(),
            COMPONENT
        );
    }

    #[test]
    fn publication_origin_uses_configured_scheme_host_and_port() {
        assert_eq!(api_origin(DEFAULT_API_URL).unwrap(), "https://console.sloper.ai");
        assert_eq!(
            api_origin("https://publisher.example:443/delivery/").unwrap(),
            "https://publisher.example"
        );
        assert_eq!(
            api_origin("http://localhost:9080/delivery/").unwrap(),
            "http://localhost:9080"
        );
        assert_eq!(
            api_origin("https://publisher.example:8443/delivery/?setting=value#fragment").unwrap(),
            "https://publisher.example:8443"
        );
        for value in ["", "relative/path", "file:///component", "data:text/plain,origin"] {
            assert_eq!(api_origin(value).unwrap_err().exit_code(), 2);
        }
    }

    #[test]
    fn credential_debug_is_redacted() {
        let options = PublishOptions {
            directory: Path::new("secret/path"),
            api_url: "https://example.invalid/secret",
            token: "secret-bearer",
            visibility: None,
        };
        let debug = format!("{options:?}");
        assert!(!debug.contains("secret"));
    }
    #[test]
    fn publication_failure_boundary_preserves_categories_without_remote_bodies() {
        for (status, expected) in [
            (401, 6),
            (403, 8),
            (404, 5),
            (409, 4),
            (422, 3),
            (429, 9),
            (503, 9),
            (500, 10),
        ] {
            let error: Error = generated_client::Error {
                kind: generated_client::ErrorKind::Status,
                message: "untrusted bearer-secret response".into(),
                status: Some(reqwest::StatusCode::from_u16(status).unwrap()),
                headers: Box::default(),
                raw_body: bytes::Bytes::from_static(b"untrusted bearer-secret response"),
                detail: None,
            }
            .into();
            assert_eq!(error.exit_code(), expected);
            let command = error.command_error();
            assert_eq!(command["details"]["status"], status);
            assert!(!command.to_string().contains("bearer-secret"));
            assert!(!format!("{error:?}").contains("bearer-secret"));
        }
        let error: Error = generated_client::Error {
            kind: generated_client::ErrorKind::Transport,
            message: "bearer-secret".into(),
            status: None,
            headers: Box::default(),
            raw_body: bytes::Bytes::new(),
            detail: None,
        }
        .into();
        assert_eq!(error.exit_code(), 9);
        assert_eq!(error.command_error()["details"], serde_json::json!({}));
        assert!(!error.command_error().to_string().contains("bearer-secret"));
    }
}
