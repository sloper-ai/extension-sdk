#![warn(missing_debug_implementations, missing_docs, rust_2018_idioms, unreachable_pub)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]

//! Asynchronous client for publishing extensions and retrieving release
//! evidence.
//!
//! ```
//! use sloper_extension_api::{
//!     Client,
//!     Config,
//! };
//!
//! let client = Client::with_config(Config::new());
//! let _resources = client.resources();
//! ```

#[path = "generated/api/mod.rs"]
mod generated;

pub use generated::*;

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io,
    };

    use serde_json::{
        Value,
        json,
    };
    use tokio::{
        io::{
            AsyncReadExt,
            AsyncWriteExt,
        },
        net::TcpListener,
        task::JoinHandle,
    };

    use crate::{
        Client,
        Config,
        CreateExtensionVersionRequest,
        Error,
        ErrorKind,
        ExtensionVersion,
        ExtensionVisibility,
        GetExtensionTrustRequest,
        GetExtensionVersionRequest,
        GetReleaseEnvelopeRequest,
        PublishExtensionVersionRequest,
        ReleaseEnvelope,
    };

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COMMON_HEADERS: &str =
        "sloper-version: 2026-09-08\r\nx-request-id: request-123\r\nratelimit: limit\r\nratelimit-policy: policy\r\n";
    const PRIVATE_CACHE_HEADERS: &str = "etag: \"release-1\"\r\ncache-control: private, no-cache\r\n";
    const PUBLIC_CACHE_HEADERS: &str = "etag: \"trust-1\"\r\ncache-control: public, max-age=300\r\n";

    async fn create_extension_version(
        client: &Client,
        extension: &str,
        key: &str,
        body: PublishExtensionVersionRequest,
    ) -> Result<Option<ExtensionVersion>, Error> {
        client
            .resources()
            .extensions()
            .create_version(CreateExtensionVersionRequest {
                extension: extension.to_owned(),
                idempotency_key: key.to_owned(),
                sloper_version: None,
                body,
            })
            .await
            .map(|response| response.body)
    }

    async fn get_release_envelope(
        client: &Client,
        extension: &str,
        digest: &str,
        etag: Option<&str>,
    ) -> Result<Option<ReleaseEnvelope>, Error> {
        client
            .resources()
            .extensions()
            .get_release_envelope(GetReleaseEnvelopeRequest {
                extension: extension.to_owned(),
                release_envelope: digest.to_owned(),
                if_none_match: etag.map(str::to_owned),
                sloper_version: None,
            })
            .await
            .map(|response| response.body)
    }

    async fn get_extension_version(
        client: &Client,
        extension: &str,
        version: &str,
        etag: Option<&str>,
    ) -> Result<Option<ExtensionVersion>, Error> {
        client
            .resources()
            .extensions()
            .get_version(GetExtensionVersionRequest {
                extension: extension.to_owned(),
                version: version.to_owned(),
                if_none_match: etag.map(str::to_owned),
                sloper_version: None,
            })
            .await
            .map(|response| response.body)
    }

    async fn get_extension_trust(client: &Client, etag: Option<&str>) -> Result<Option<String>, Error> {
        let response = client
            .resources()
            .extension_trust()
            .get(GetExtensionTrustRequest {
                if_none_match: etag.map(str::to_owned),
                sloper_version: None,
            })
            .await?;
        Ok(response.body)
    }

    fn client(origin: impl AsRef<str>) -> Client {
        let origin = origin.as_ref();
        let mut config = Config::new()
            .with_api_base(format!("{}/api/v1", origin.trim_end_matches('/')))
            .with_api_key("scope-token".to_owned().into());
        config.headers.insert("origin", origin.parse().unwrap());
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Client::with_http_client(config, http)
    }

    fn body() -> PublishExtensionVersionRequest {
        PublishExtensionVersionRequest {
            component: b"component-bytes".to_vec(),
            icon: None,
            visibility: None,
            additional_properties: BTreeMap::new(),
        }
    }

    fn version() -> Value {
        json!({
            "extension_id": "sloper.example",
            "version": "1.0.0",
            "state": "published",
            "published_at": "2026-09-07T12:00:00Z",
            "release_envelope_id": DIGEST,
            "reason": null,
        })
    }

    async fn server(status: &str, headers: &str, body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind local test server");
        let address = listener.local_addr().expect("read bound local address");
        let response = format!(
            "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = socket.read(&mut buffer).await.expect("read request");
                assert_ne!(count, 0, "client closed before submitting the request");
                bytes.extend_from_slice(&buffer[..count]);
                let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let header = String::from_utf8_lossy(&bytes[..end]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("valid content length"))
                    })
                    .unwrap_or_default();
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.expect("write response");
            socket.shutdown().await.expect("close response");
            String::from_utf8(bytes).expect("test request uses UTF-8 data")
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn multipart_publication_preserves_bytes_audience_and_request_identity() {
        let headers = format!(
            "{COMMON_HEADERS}content-type: application/json\r\nlocation: https://console.sloper.ai/api/v1/extensions/sloper.example/versions/1.0.0\r\n"
        );
        let (origin, request) = server("201 Created", &headers, &version().to_string()).await;
        let client = client(format!("{origin}/"));
        let form = PublishExtensionVersionRequest {
            visibility: Some(ExtensionVisibility::Private),
            icon: Some(b"icon-bytes".to_vec()),
            ..body()
        };
        let response = create_extension_version(&client, "sloper.example", "attempt-123", form)
            .await
            .expect("publish succeeds");
        let published = response.expect("created version body");
        assert_eq!(published.extension_id, "sloper.example");
        assert_eq!(published.version, "1.0.0");
        assert_eq!(published.release_envelope_id, DIGEST);
        let request = request.await.expect("request server succeeds");
        assert!(
            request.starts_with("POST /api/v1/extensions/sloper.example/versions HTTP/1.1\r\n"),
            "request line: {:?}",
            request.lines().next()
        );
        assert!(request.contains("authorization: Bearer scope-token\r\n"));
        assert!(request.contains("idempotency-key: attempt-123\r\n"));
        assert!(request.contains("sloper-version: 2026-09-08\r\n"));
        assert!(request.contains("name=\"component\"; filename=\"component\""));
        assert!(request.contains("component-bytes"));
        assert!(request.contains("name=\"visibility\""));
        assert!(request.contains("\r\n\r\nprivate\r\n"));
        assert!(request.contains("name=\"icon\"; filename=\"icon\""));
        assert!(request.contains("icon-bytes"));
        assert!(request.to_ascii_lowercase().contains("content-type: image/png"));
    }

    #[tokio::test]
    async fn rejection_preserves_declared_problem_status_and_body() {
        let problem = json!({"type":"about:blank", "title":"Conflict", "status":409, "detail":"Version already exists.", "code":"version_conflict"});
        let (origin, request) = server(
            "409 Conflict",
            "content-type: application/problem+json\r\n",
            &problem.to_string(),
        )
        .await;
        let error = create_extension_version(&client(origin), "sloper.example", "attempt-123", body())
            .await
            .expect_err("different bytes cannot replace a version");
        let response = error;
        assert_eq!(response.status.unwrap().as_u16(), 409);
        assert_eq!(response.raw_body.as_ref(), problem.to_string().as_bytes());
        assert!(
            serde_json::from_slice::<serde_json::Value>(&response.raw_body)
                .is_ok_and(|problem| problem["code"] == "version_conflict")
        );
        request.await.expect("request server succeeds");
    }

    #[tokio::test]
    async fn malformed_success_retains_body_and_reports_spec_failure() {
        let headers = format!("{COMMON_HEADERS}{PRIVATE_CACHE_HEADERS}content-type: application/json\r\n");
        let (origin, request) = server("200 OK", &headers, "{}").await;
        let error = get_extension_version(&client(origin), "sloper.example", "1.0.0", None::<&str>)
            .await
            .expect_err("missing fields must be rejected");
        let response = error;
        assert_eq!(response.status.unwrap().as_u16(), 200);
        assert_eq!(response.raw_body.as_ref(), b"{}");
        assert_eq!(response.kind, ErrorKind::Decode);
        request.await.expect("request server succeeds");
    }

    #[tokio::test]
    async fn version_reason_preserves_required_nullable_contract_before_model_decoding() {
        let headers = format!("{COMMON_HEADERS}{PRIVATE_CACHE_HEADERS}content-type: application/json\r\n");
        for reason in [Value::Null, json!("withdrawn")] {
            let mut body = version();
            body["reason"] = reason;
            let (origin, request) = server("200 OK", &headers, &body.to_string()).await;
            get_extension_version(&client(origin), "sloper.example", "1.0.0", None)
                .await
                .expect("required nullable reason is present");
            request.await.expect("request server succeeds");
        }
        let mut absent = version();
        absent.as_object_mut().expect("version object").remove("reason");
        let mut invalid = version();
        invalid["reason"] = json!(42);
        for body in [absent, invalid] {
            let (origin, request) = server("200 OK", &headers, &body.to_string()).await;
            let error = get_extension_version(&client(origin), "sloper.example", "1.0.0", None)
                .await
                .expect_err("canonical contract rejects omitted or invalid reason");
            assert_eq!(error.kind, ErrorKind::Decode);
            request.await.expect("request server succeeds");
        }
    }

    #[tokio::test]
    async fn signed_trust_text_with_charset_is_preserved_exactly() {
        let trust = "sloper-extension-trust-v1 eyJzZXF1ZW5jZSI6MX0.signature\n";
        let headers = format!("{COMMON_HEADERS}{PUBLIC_CACHE_HEADERS}content-type: text/plain; charset=utf-8\r\n");
        let (origin, request) = server("200 OK", &headers, trust).await;
        let response = get_extension_trust(&client(origin), None::<&str>)
            .await
            .unwrap_or_else(|error| panic!("signed text error: {:?}", error.message));
        assert_eq!(response.as_deref(), Some(trust));
        let request = request.await.expect("request server succeeds");
        assert!(request.starts_with("GET /api/v1/extension-trust HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn not_modified_preserves_cache_headers_and_has_no_body() {
        let headers = format!("{COMMON_HEADERS}{PRIVATE_CACHE_HEADERS}");
        let (origin, request) = server("304 Not Modified", &headers, "").await;
        let response = get_extension_version(&client(origin), "sloper.example", "1.0.0", Some("\"release-1\""))
            .await
            .expect("declared conditional response succeeds");
        assert!(response.is_none());
        let request = request.await.expect("request server succeeds");
        assert!(request.starts_with("GET /api/v1/extensions/sloper.example/versions/1.0.0 HTTP/1.1\r\n"));
        assert!(request.contains("if-none-match: \"release-1\"\r\n"));
    }

    #[tokio::test]
    async fn redirects_are_not_followed_with_publishing_credentials() {
        let (origin, request) = server(
            "307 Temporary Redirect",
            "location: http://127.0.0.1:1/untrusted\r\n",
            "",
        )
        .await;
        let error = create_extension_version(&client(&origin), "sloper.example", "attempt-123", body())
            .await
            .expect_err("redirect cannot forward credentials");
        assert_eq!(error.status.unwrap().as_u16(), 307);
        let request = request.await.expect("request server succeeds");
        assert!(request.contains("authorization: Bearer scope-token\r\n"));
        assert!(request.contains(&format!("origin: {origin}\r\n")));
    }

    #[tokio::test]
    async fn connection_failure_is_a_transport_error() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        drop(listener);
        let error = get_extension_trust(&client(format!("http://{address}")), None::<&str>)
            .await
            .expect_err("closed listener refuses connection");
        assert!(error.status.is_none());
        assert_eq!(error.kind, ErrorKind::Transport);
        Ok(())
    }

    #[tokio::test]
    async fn release_evidence_preserves_manifest_envelope_and_download_descriptor() {
        let manifest = "{\"id\":\"sloper.example\"}\n";
        let envelope = "sloper-extension-release-v1 exact-signed-envelope\n";
        let body = json!({
            "id": DIGEST,
            "extension_id": "sloper.example",
            "version": "1.0.0",
            "publisher": {"id":"sloper", "kind":"organization", "handle":"sloper", "name":"Sloper"},
            "manifest": manifest,
            "envelope": envelope,
            "component": {"url":"https://artifacts.example/extension.wasm", "sha256":DIGEST, "bytes":17, "expires_at":"2026-09-07T13:00:00Z"},
        });
        let headers = format!("{COMMON_HEADERS}{PRIVATE_CACHE_HEADERS}content-type: application/json\r\n");
        let (origin, request) = server("200 OK", &headers, &body.to_string()).await;
        let response = get_release_envelope(&client(origin), "sloper.example", DIGEST, None::<&str>)
            .await
            .expect("release evidence succeeds");
        let release = response.expect("release evidence body");
        assert_eq!(release.id, DIGEST);
        assert_eq!(release.manifest, manifest);
        assert_eq!(release.envelope, envelope);
        assert_eq!(release.component.sha256, DIGEST);
        assert_eq!(release.component.bytes, 17);
        assert_eq!(release.publisher.id, "sloper");
        let request = request.await.expect("request server succeeds");
        assert!(request.starts_with(&format!(
            "GET /api/v1/extensions/sloper.example/release-envelopes/sha256%3A{} HTTP/1.1\r\n",
            "a".repeat(64)
        )));
    }

    #[tokio::test]
    async fn undeclared_success_is_not_treated_as_a_completed_publication() {
        let (origin, request) = server("202 Accepted", COMMON_HEADERS, &version().to_string()).await;
        let error = create_extension_version(&client(origin), "sloper.example", "attempt-123", body())
            .await
            .expect_err("publication requires a declared completed response");
        assert_eq!(error.status.unwrap().as_u16(), 202);
        request.await.expect("request server succeeds");
    }

    #[tokio::test]
    async fn missing_required_cache_header_rejects_an_otherwise_valid_response() {
        let headers = format!("{COMMON_HEADERS}content-type: application/json\r\n");
        let (origin, request) = server("200 OK", &headers, &version().to_string()).await;
        let error = get_extension_version(&client(origin), "sloper.example", "1.0.0", None::<&str>)
            .await
            .expect_err("required cache metadata must be present");
        let response = error;
        assert_eq!(response.status.unwrap().as_u16(), 200);
        assert_eq!(response.kind, ErrorKind::Decode);
        assert!(response.message.contains("header"));
        request.await.expect("request server succeeds");
    }
    #[tokio::test]
    async fn generated_constraints_reject_invalid_formats_and_cache_metadata() {
        let headers = format!("{COMMON_HEADERS}{PRIVATE_CACHE_HEADERS}content-type: application/json\r\n");
        let mut invalid = version();
        invalid["published_at"] = json!("yesterday");
        let (origin, request) = server("200 OK", &headers, &invalid.to_string()).await;
        let error = get_extension_version(&client(origin), "sloper.example", "1.0.0", None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Decode);
        assert_eq!(error.raw_body.as_ref(), invalid.to_string().as_bytes());
        request.await.unwrap();

        let headers =
            format!("{COMMON_HEADERS}etag: opaque\r\ncache-control: public\r\ncontent-type: application/json\r\n");
        let (origin, request) = server("200 OK", &headers, &version().to_string()).await;
        let error = get_extension_version(&client(origin), "sloper.example", "1.0.0", None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Decode);
        assert_eq!(error.headers["cache-control"], "public");
        request.await.unwrap();
    }

    #[tokio::test]
    async fn cache_tags_follow_the_opaque_string_contract() {
        let headers =
            format!("{COMMON_HEADERS}etag:\r\ncache-control: private, no-cache\r\ncontent-type: application/json\r\n");
        let (origin, request) = server("200 OK", &headers, &version().to_string()).await;
        let response = get_extension_version(&client(origin), "sloper.example", "1.0.0", None)
            .await
            .expect("an opaque string has no generated length constraint");
        assert_eq!(response.expect("version body").extension_id, "sloper.example");
        request.await.unwrap();
    }
}
