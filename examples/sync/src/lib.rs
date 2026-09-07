//! Read and write the same declared resource with a stable item key.

use futures::{
    SinkExt,
    TryStreamExt,
};
use serde::{
    Deserialize,
    Serialize,
};
use sloper_extension::{
    Reader,
    Resource,
    Result,
    Writer,
    action,
    extension,
};

#[derive(Deserialize, Serialize, Resource)]
#[resource(name = "invoices", key = id)]
struct Invoice {
    #[schema(max_length = 512)]
    id: String,
    label: String,
}

/// Trim whitespace from invoice labels.
#[action]
async fn normalize(mut input: Reader<Invoice>, mut output: Writer<Invoice>) -> Result<()> {
    while let Some(invoice) = input.try_next().await? {
        let normalized = Invoice {
            id: invoice.id.clone(),
            label: invoice.label.trim().to_owned(),
        };
        output.send(vec![normalized].into()).await?;
    }
    Ok(())
}

extension! {
    name: "acme.invoice-labels",
    actions: [normalize],
}
