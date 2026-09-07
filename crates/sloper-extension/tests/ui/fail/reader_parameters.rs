use serde::{Deserialize, Serialize};
use sloper_extension::{action, Reader, Resource, Result, Schema};

#[derive(Deserialize, Serialize, Resource)]
#[resource(name = "invoices")]
struct Invoice { id: String }

#[derive(Deserialize, Schema)]
struct Parameters { name: String }

#[action]
async fn normalize(_input: Reader<Invoice>, _parameters: Parameters) -> Result<()> { Ok(()) }

fn main() {}
