//! Real SDK actions used by the component dispatch conformance tests.

use std::{
    net::ToSocketAddrs,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures::{
    Sink,
    SinkExt,
    StreamExt,
    TryStreamExt,
};
use serde::{
    Deserialize,
    Serialize,
};
use sloper_extension::{
    Batch,
    Connection,
    Error,
    Reader,
    Resource,
    Result,
    Schema,
    Source,
    Writer,
    action,
    extension,
};

#[derive(sloper_extension::Connection)]
#[connection(name = "ledger", profile = "acme.ledger", scopes = ["items.read"])]
struct Ledger;

#[derive(Deserialize, Serialize, Resource, Clone)]
#[resource(name = "items", key = id)]
struct Item {
    #[schema(max_length = 512)]
    id: String,
    value: Option<f64>,
    payload: String,
    #[schema(max_bytes = 131072)]
    document: Option<Source>,
}

async fn check_tokens(connection: &Connection<Ledger>) -> Result<()> {
    let first = connection.access_token().await?;
    let second = connection.access_token().await?;
    assert!(!format!("{first:?}").contains(first.expose_secret()));
    assert!(!format!("{second:?}").contains(second.expose_secret()));
    assert_ne!(first.expose_secret(), second.expose_secret());
    assert_eq!(first.scopes(), ["items.read"]);
    assert_eq!(first.scopes(), second.scopes());
    log::debug!("tokens: {first:?}, {second:?}");
    Ok(())
}

#[action]
async fn chunk_items(mut output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    check_tokens(&ledger).await?;
    let items = (0_u32..2001)
        .map(|index| {
            Item {
                id: format!("i{index}"),
                value: Some(f64::from(index)),
                payload: "x".into(),
                document: None,
            }
        })
        .collect();
    output.send(Batch::with_cursor(items, "c2")).await
}

#[action]
async fn chunk_bytes(mut output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    check_tokens(&ledger).await?;
    let payload = "x".repeat(900 * 1024);
    let items = (0..10)
        .map(|index| {
            Item {
                id: format!("item-{index}"),
                value: Some(1.0),
                payload: payload.clone(),
                document: None,
            }
        })
        .collect();
    output.send(Batch::with_cursor(items, "c2")).await
}

#[action]
async fn empty_cursor(mut output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    check_tokens(&ledger).await?;
    output.send(Batch::with_cursor(Vec::new(), "c2")).await
}

fn bounded_item(excess: usize) -> Item {
    let overhead = r#"{"id":"boundary","value":null,"payload":"","document":null}"#.len();
    Item {
        id: "boundary".into(),
        value: None,
        payload: "x".repeat(1024 * 1024 - overhead + excess),
        document: None,
    }
}

#[action]
async fn exact_item(mut output: Writer<Item>) -> Result<()> {
    output.send(vec![bounded_item(0)].into()).await
}

#[action]
async fn oversized_item(mut output: Writer<Item>) -> Result<()> {
    let result = output.send(vec![bounded_item(1)].into()).await;
    assert!(matches!(result, Err(Error::TooLarge)));
    log::info!("ignored oversized item error; action returns success");
    Ok(())
}

#[action]
async fn dropped_send(mut output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    check_tokens(&ledger).await?;
    let batch = vec![Item {
        id: "dropped".into(),
        value: Some(1.0),
        payload: "x".into(),
        document: None,
    }]
    .into();
    Sink::start_send(Pin::new(&mut output), batch)?;
    drop(output);
    Ok(())
}

#[action]
async fn read_none(_input: Reader<Item>) -> Result<()> {
    Ok(())
}

#[action]
async fn read_one(mut input: Reader<Item>) -> Result<()> {
    let item = input.next().await.transpose()?.expect("first committed item");
    assert_eq!(item.seed(), "seed-a");
    assert_eq!(item.id, "a");
    Ok(())
}

#[action]
async fn read_all(mut input: Reader<Item>) -> Result<()> {
    let mut count = 0;
    while let Some(item) = input.try_next().await? {
        assert_eq!(item.seed(), "seed-a");
        assert_eq!(item.id, "a");
        count += 1;
    }
    assert_eq!(count, 1);
    Ok(())
}

#[action]
async fn pointer_source(output: Writer<Item>) -> Result<()> {
    let result = output.source("/properties/document", "alias.bin").await;
    assert!(matches!(result, Err(Error::Internal(_))));
    log::info!("ignored source property error; action returns success");
    Ok(())
}

#[action]
async fn nonfinite(mut output: Writer<Item>) -> Result<()> {
    let batch = vec![Item {
        id: "nan".into(),
        value: Some(f64::NAN),
        payload: "x".into(),
        document: None,
    }]
    .into();
    let result = output.send(batch).await;
    assert!(matches!(result, Err(Error::Internal(_))));
    log::info!("ignored nonfinite item error; action returns success");
    Ok(())
}

#[derive(Deserialize, Schema)]
struct Endpoint {
    address: String,
}

#[derive(Debug)]
struct WasiDns;

impl reqwest::dns::Resolve for WasiDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        // std resolves through WASI here; no unsupported blocking thread pool.
        Box::pin(async move {
            let addresses = (name.as_str(), 0).to_socket_addrs()?;
            Ok(Box::new(addresses) as reqwest::dns::Addrs)
        })
    }
}

/// Runs ordinary crate I/O while a component-model source and writer are
/// pending.
#[action]
async fn crate_compatibility(endpoint: Endpoint, mut output: Writer<Item>) -> Result<()> {
    let before = std::time::Instant::now();
    let timer = tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        42
    });
    let random = rand::random::<[u8; 32]>();
    let mut entropy = [0; 32];
    getrandom::fill(&mut entropy).map_err(|_| Error::internal("randomness is unavailable"))?;
    assert_ne!(random, entropy, "independent entropy requests returned identical bytes");
    let path = std::env::current_dir()?.join("crate-compatibility.bin");
    std::fs::write(&path, random)?;
    assert_eq!(std::fs::read(&path)?, random);
    assert_eq!(std::fs::read("crate-compatibility.bin")?, random);
    assert!(std::fs::read("/etc/passwd").is_err(), "host paths must not be visible");
    assert!(
        std::net::TcpListener::bind("127.0.0.1:0").is_err(),
        "the guest must not accept inbound connections"
    );
    assert!(
        ("localhost", 80).to_socket_addrs()?.next().is_some(),
        "the WASI host must resolve localhost"
    );

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|_| Error::internal("HTTP client initialization failed"))?;
    let request = async {
        let response = client
            .get(format!("http://{}/", endpoint.address))
            .send()
            .await
            .map_err(|_| Error::internal("HTTP request failed"))?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response
            .text()
            .await
            .map_err(|_| Error::internal("HTTP response failed"))?;
        assert_eq!(body, "crate-compatible");
        Ok(())
    };
    let write = async {
        let mut source = output.source("document", "entropy.bin").await?;
        futures::io::AsyncWriteExt::write_all(&mut source, &entropy).await?;
        let document = source.close().await?;
        output
            .send(Batch::with_cursor(
                vec![Item {
                    id: "crate-compatibility".into(),
                    value: Some(42.0),
                    payload: "mixed async runtimes".into(),
                    document: Some(document),
                }],
                "mixed-complete",
            ))
            .await
    };
    let (requested, written): (Result<()>, Result<()>) = futures::join!(request, write);
    requested?;
    written?;
    let address: std::net::SocketAddr = endpoint
        .address
        .parse()
        .map_err(|_| Error::internal("test endpoint is invalid"))?;
    let response = reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(WasiDns))
        .build()
        .map_err(|_| Error::internal("HTTP client initialization failed"))?
        .get(format!("http://localhost:{}/", address.port()))
        .send()
        .await
        .map_err(|_| Error::internal("HTTP hostname request failed"))?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .text()
            .await
            .map_err(|_| Error::internal("HTTP hostname response failed"))?,
        "crate-compatible"
    );
    assert_eq!(timer.await.map_err(|_| Error::internal("timer task failed"))?, 42);
    assert!(before.elapsed() >= Duration::from_millis(10));
    std::fs::remove_file(path)?;
    Ok(())
}

/// Verifies the host-managed WASI HTTP route in the same generated dispatch.
#[action]
async fn wasi_http(endpoint: Endpoint) -> Result<()> {
    wstd::runtime::block_on(async {
        // wstd 0.6.8's Body::empty writes zero bytes after starting the GET,
        // which can race completion of its zero-length outgoing body. An empty
        // stream finishes the body without issuing a data write.
        let request = wstd::http::Request::get(format!("http://{}/", endpoint.address))
            .body(wstd::http::Body::from_stream(futures::stream::empty::<Vec<u8>>()))
            .map_err(|_| Error::internal("HTTP request construction failed"))?;
        let mut response = wstd::http::Client::new().send(request).await.map_err(|error| {
            log::error!("WASI HTTP request failed: {error:?}");
            Error::internal("WASI HTTP request failed")
        })?;
        assert_eq!(response.status(), wstd::http::StatusCode::OK);
        let body = response.body_mut().str_contents().await.map_err(|error| {
            log::error!("WASI HTTP response failed: {error:?}");
            Error::internal("WASI HTTP response failed")
        })?;
        assert_eq!(body, "crate-compatible");
        Ok(())
    })
}

/// A request whose peer never responds remains cancellable by the host.
#[action]
async fn pending_http(endpoint: Endpoint) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|_| Error::internal("HTTP client initialization failed"))?;
    client
        .get(format!("http://{}/", endpoint.address))
        .send()
        .await
        .map_err(|_| Error::internal("HTTP request failed"))?;
    Err(Error::internal("the test peer must not respond"))
}

#[action]
async fn abandoned_source(output: Writer<Item>) -> Result<()> {
    let mut source = output.source("document", "entropy.bin").await?;
    futures::io::AsyncWriteExt::write_all(&mut source, b"partial upload").await?;
    futures::io::AsyncWriteExt::flush(&mut source).await?;
    drop(source);
    Ok(())
}

extension! {
    name: "acme.sdk-conformance",
    actions: [chunk_items, chunk_bytes, empty_cursor, exact_item, oversized_item, dropped_send, read_none, read_one, read_all, pointer_source, nonfinite, crate_compatibility, wasi_http, pending_http, abandoned_source],
}
