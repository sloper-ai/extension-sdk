use serde::Deserialize;
use sloper_extension::{action, extension, Result, Schema};
use std::collections::BTreeMap;

#[derive(Deserialize, Schema)]
struct Configuration { values: BTreeMap<String, String> }

#[action]
async fn echo() -> Result<()> { Ok(()) }

extension! { name: "acme.demo", configuration: Configuration, actions: [echo] }

fn main() {}
