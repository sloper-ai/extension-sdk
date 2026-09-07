mod support {
    pub(crate) mod host_fixture;
}

use std::{
    fs,
    sync::{
        Arc,
        atomic::Ordering,
    },
    time::Duration,
};

use sloper_extension_host::Engine;
use support::host_fixture::{
    TestHost,
    request,
    run,
};
use tokio::{
    io::{
        AsyncBufReadExt,
        AsyncReadExt,
        AsyncWriteExt,
        BufReader,
    },
    net::TcpListener,
    time::timeout,
};

#[tokio::test]
async fn shared_wasi_context_allows_dns_clocks_randomness_and_denies_inherited_io_and_listeners() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    run(&engine, request("probe"), Arc::clone(&host), None)
        .await
        .expect("WASI capability probes complete");
    assert_eq!(host.finishes.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn scratch_filesystem_is_writable_fresh_per_run_and_cannot_access_host_files() {
    let sentinel = tempfile::NamedTempFile::new().expect("host sentinel file creates");
    fs::write(sentinel.path(), b"private host content").expect("host sentinel content writes");
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    for _ in 0..2 {
        let mut input = request("scratch");
        input.parameters = serde_json::json!({
            "hostPath": sentinel.path(),
            "hostFilename": sentinel.path().file_name().expect("sentinel has a filename").to_str(),
        })
        .to_string();
        run(&engine, input, Arc::clone(&host), None)
            .await
            .expect("scratch stdlib operations succeed with host files denied");
        assert_eq!(
            fs::read(sentinel.path()).expect("host sentinel stays readable"),
            b"private host content",
            "creating a scratch file with the sentinel's name must not overwrite the host file"
        );
    }
    assert_eq!(host.finishes.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn outgoing_tcp_connects_and_exchanges_bytes_through_the_standard_library() {
    let server = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback test server binds");
    let address = server.local_addr().expect("bound server has an address").to_string();
    let responder = tokio::spawn(async move {
        let (mut socket, _) = server.accept().await.expect("guest connects through std TCP");
        let mut received = [0; 9];
        socket.read_exact(&mut received).await.expect("TCP request arrives");
        assert_eq!(&received, b"guest-tcp");
        socket.write_all(b"host-tcp").await.expect("TCP response writes");
    });
    let engine = Engine::new().expect("engine config is valid");
    let mut input = request("tcp");
    input.parameters = serde_json::json!({"address": address}).to_string();
    timeout(
        Duration::from_secs(20),
        run(&engine, input, Arc::new(TestHost::default()), None),
    )
    .await
    .expect("TCP action finishes promptly")
    .expect("standard-library TCP succeeds");
    responder.await.expect("TCP server task succeeds");
}

#[tokio::test]
async fn outgoing_wasi_http_connects_and_exchanges_a_response() {
    let server = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback test server binds");
    let authority = server.local_addr().expect("bound server has an address").to_string();
    let responder = tokio::spawn(async move {
        let (mut socket, _) = server.accept().await.expect("guest connects through WASI HTTP");
        let mut request_line = String::new();
        BufReader::new(&mut socket)
            .read_line(&mut request_line)
            .await
            .expect("HTTP request line arrives");
        assert_eq!(request_line, "GET /host-test HTTP/1.1\r\n");
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nhost-http")
            .await
            .expect("HTTP response writes");
    });
    let engine = Engine::new().expect("engine config is valid");
    let mut input = request("http");
    input.parameters = serde_json::json!({"authority": authority}).to_string();
    timeout(
        Duration::from_secs(20),
        run(&engine, input, Arc::new(TestHost::default()), None),
    )
    .await
    .expect("HTTP action finishes promptly")
    .expect("WASI HTTP succeeds");
    responder.await.expect("HTTP server task succeeds");
}
