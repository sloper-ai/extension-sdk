use serde::Deserialize;
use sloper_extension::{action, extension, Fields, Result, Schema};

#[derive(Deserialize, Schema)]
struct Configuration { #[serde(flatten)] fields: Fields }

#[action]
async fn echo() -> Result<()> { Ok(()) }

extension! { name: "acme.demo", configuration: Configuration, actions: [echo] }

fn main() {}
