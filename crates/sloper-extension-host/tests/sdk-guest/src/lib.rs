#![cfg(target_arch = "wasm32")]

use futures::{
    SinkExt,
    TryStreamExt,
    io::AsyncWriteExt,
};
use serde::{
    Deserialize,
    Serialize,
};
use sloper_extension::{
    Connection,
    Error,
    Reader,
    Resource,
    Result,
    Source,
    Writer,
    action,
    extension,
};
use time::{
    OffsetDateTime,
    format_description::well_known::Rfc3339,
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

async fn log_tokens(conn: &Connection<Ledger>) -> Result<()> {
    let first = conn.access_token().await?;
    let second = conn.access_token().await?;
    log::debug!("tokens: {first:?}, {second:?}");
    Ok(())
}

#[action]
async fn produce(mut output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    log_tokens(&ledger).await?;
    let mut source = output.source("document", "document.bin").await?;
    let bytes = vec![7_u8; 100_000];
    source.write_all(&bytes).await?;
    let source = source.close().await?;
    output
        .send(
            vec![Item {
                id: "produced".into(),
                value: Some(1.0),
                payload: "x".into(),
                document: Some(source),
            }]
            .into(),
        )
        .await
}

#[action]
async fn oversized(output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    log_tokens(&ledger).await?;
    let mut source = output.source("document", "too.bin").await?;
    let _ = source.write_all(&vec![0_u8; 131073]).await;
    drop(source);
    Ok(())
}

#[action]
async fn abandoned(output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    log_tokens(&ledger).await?;
    let mut source = output.source("document", "abandoned.bin").await?;
    source.write_all(b"x").await?;
    drop(source);
    Ok(())
}

#[action]
async fn oversized_after_prefix(output: Writer<Item>) -> Result<()> {
    let mut source = output.source("document", "prefix.bin").await?;
    source.write_all(&vec![7; 64 * 1024]).await?;
    let _ = source.write_all(&vec![8; 131072]).await;
    let _ = source.close().await;
    Ok(())
}

#[action]
async fn forgotten_upload(output: Writer<Item>) -> Result<()> {
    let mut source = output.source("document", "forgotten.bin").await?;
    source.write_all(b"unsealed prefix").await?;
    std::mem::forget(source);
    Ok(())
}

#[action]
async fn seal_then_failure(output: Writer<Item>) -> Result<()> {
    let mut source = output.source("document", "durable.bin").await?;
    source.write_all(b"durable contents").await?;
    let _sealed = source.close().await?;
    Err(Error::rejected("later action failure"))
}

#[action]
async fn source_copy(mut input: Reader<Item>, mut output: Writer<Item>, ledger: Connection<Ledger>) -> Result<()> {
    log_tokens(&ledger).await?;
    if let Some(mut item) = input.try_next().await? {
        if let Some(source) = item.document.take() {
            let mut reader = source.open().await?;
            let mut writer = output.source("document", "copied.bin").await?;
            futures::io::copy_buf(&mut reader, &mut writer).await?;
            item.document = Some(writer.close().await?);
        }
        output
            .send(
                vec![Item {
                    id: item.id.clone(),
                    value: item.value,
                    payload: item.payload.clone(),
                    document: item.document.clone(),
                }]
                .into(),
            )
            .await?;
    }
    Ok(())
}

#[action]
async fn retry_failure() -> Result<()> {
    Err(Error::unavailable(
        "provider requested retry",
        Some(OffsetDateTime::parse("2099-01-02T03:04:05.123456789Z", &Rfc3339).expect("valid fixture timestamp")),
    ))
}

extension! {
    name: "acme.sdk-spec",
    actions: [produce, oversized, abandoned, source_copy, retry_failure, oversized_after_prefix, forgotten_upload, seal_then_failure],
}
