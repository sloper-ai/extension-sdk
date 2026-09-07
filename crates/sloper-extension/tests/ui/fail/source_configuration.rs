use serde::Deserialize;
use sloper_extension::{action, extension, Result, Schema, Source};

#[derive(Deserialize, Schema)]
struct Configuration { document: Source }

#[action]
async fn echo() -> Result<()> { Ok(()) }

extension! { name: "acme.demo", configuration: Configuration, actions: [echo] }

fn main() {}
