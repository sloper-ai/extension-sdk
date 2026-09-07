//! A foreground action with typed parameters and no resource capabilities.

use serde::Deserialize;
use sloper_extension::{
    Result,
    Schema,
    action,
    extension,
};

#[derive(Deserialize, Schema)]
struct Greeting {
    #[schema(max_length = 100)]
    name: String,
}

/// Log a greeting to the requested name.
#[action]
async fn echo(greeting: Greeting) -> Result<()> {
    log::info!("Hello, {}", greeting.name);
    Ok(())
}

extension! {
    name: "acme.echo",
    label: "Echo",
    actions: [echo],
}
