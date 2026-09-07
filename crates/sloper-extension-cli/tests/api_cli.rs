use std::{
    process::Output,
    time::Duration,
};

use serde_json::{
    Value,
    json,
};
use tokio::{
    io::{
        AsyncReadExt as _,
        AsyncWriteExt as _,
    },
    net::TcpListener,
    process::Command,
    time::timeout,
};

const BINARY: &str = env!("CARGO_BIN_EXE_sloper-extension");
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
// A failed child or missing request must terminate the local peer as well.
const DEADLINE: Duration = Duration::from_secs(20);

async fn exchange(arguments: &[&str], status: &str, headers: &str, body: &str) -> (Output, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let request = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "client closed before completing its request");
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or_default();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(response.as_bytes()).await.unwrap();
        request
    };
    let mut command = Command::new(BINARY);
    command
        .args(["--json", "api", "--api-url", &origin])
        .args(arguments)
        .env("SLOPER_API_TOKEN", "fixture-token")
        .env_remove("SLOPER_API_URL")
        .kill_on_drop(true);
    let (output, request) = timeout(DEADLINE, async { tokio::join!(command.output(), request) })
        .await
        .unwrap();
    (output.unwrap(), request)
}

#[tokio::test]
async fn api_help_is_generated_and_available_without_credentials() {
    let output = Command::new(BINARY)
        .args(["api", "extensions", "create-extension-version", "--help"])
        .env_remove("SLOPER_API_TOKEN")
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--component"));
    assert!(help.contains("--idempotency-key"));
}

#[tokio::test]
async fn generated_version_command_sends_context_and_prints_the_response() {
    let version = json!({"extension_id":"sloper.example", "version":"1.0.0", "state":"published",
        "published_at":"2026-09-07T12:00:00Z", "release_envelope_id":DIGEST, "reason":null});
    let (output, request) = exchange(
        &[
            "extensions",
            "get-extension-version",
            "--extension",
            "sloper.example",
            "--version",
            "1.0.0",
        ],
        "200 OK",
        "Content-Type: application/json\r\nSloper-Version: 2026-09-08\r\nX-Request-Id: fixture\r\nRateLimit: \
         limit\r\nRateLimit-Policy: policy\r\nETag: \"v1\"\r\nCache-Control: private, no-cache\r\n",
        &version.to_string(),
    )
    .await;
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout).unwrap(), version);
    let request = String::from_utf8(request).unwrap();
    assert!(request.starts_with("GET /api/v1/extensions/sloper.example/versions/1.0.0 HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer fixture-token\r\n"));
    assert!(request.contains("sloper-version: 2026-09-08\r\n"));
    assert!(request.contains("origin: http://127.0.0.1:"));
}

#[tokio::test]
async fn generated_publish_reads_binary_files_and_preserves_conflict_exit_codes() {
    let directory = tempfile::tempdir().unwrap();
    let component = directory.path().join("component.wasm");
    let bytes = [0, 255, 1, 2];
    std::fs::write(&component, bytes).unwrap();
    let problem = json!({"type":"about:blank", "title":"Conflict", "status":409,
        "detail":"Version already exists", "code":"version_conflict"});
    let (output, request) = exchange(
        &[
            "extensions",
            "create-extension-version",
            "--extension",
            "sloper.example",
            "--idempotency-key",
            "fixture-request",
            "--component",
            component.to_str().unwrap(),
        ],
        "409 Conflict",
        "Content-Type: application/problem+json\r\n",
        &problem.to_string(),
    )
    .await;
    assert_eq!(output.status.code(), Some(4));
    let output: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["error"]["details"]["code"], "version_conflict");
    assert!(!output.to_string().contains("fixture-token"));
    assert!(request.windows(bytes.len()).any(|window| window == bytes));
    let request = String::from_utf8_lossy(&request);
    assert!(request.starts_with("POST /api/v1/extensions/sloper.example/versions HTTP/1.1\r\n"));
    assert!(request.contains("idempotency-key: fixture-request\r\n"));
    assert!(request.contains("name=\"component\""));
}

#[tokio::test]
async fn invalid_api_input_is_a_usage_failure_before_network_access() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(BINARY)
        .args([
            "--json",
            "api",
            "--api-url",
            "http://127.0.0.1:1",
            "extensions",
            "create-extension-version",
            "--extension",
            "sloper.example",
            "--idempotency-key",
            "fixture",
            "--component",
        ])
        .arg(directory.path().join("missing.wasm"))
        .env("SLOPER_API_TOKEN", "fixture-token")
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "USAGE");
    assert!(!error.to_string().contains("fixture-token"));
}

#[tokio::test]
async fn malformed_api_json_is_a_response_failure_with_its_http_status() {
    let (output, _) = exchange(
        &[
            "extensions",
            "get-extension-version",
            "--extension",
            "sloper.example",
            "--version",
            "1.0.0",
        ],
        "200 OK",
        "Content-Type: Application/JSON\r\nSloper-Version: 2026-09-08\r\nX-Request-Id: fixture\r\nRateLimit: \
         limit\r\nRateLimit-Policy: policy\r\nETag: \"v1\"\r\nCache-Control: private, no-cache\r\n",
        "{invalid-secret-body",
    )
    .await;
    assert_eq!(output.status.code(), Some(10));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["details"]["status"], 200);
    assert!(!error.to_string().contains("invalid-secret-body"));
}
