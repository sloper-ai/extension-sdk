use serde::{Deserialize, Serialize};
use sloper_extension::{action, Resource, Result, Writer};

#[derive(Deserialize, Serialize, Resource)]
#[resource(name = "invoices")]
struct Invoice { id: String }

#[action]
async fn normalize(_output: Writer<Invoice>) -> Result<()> { Ok(()) }

fn main() {}
