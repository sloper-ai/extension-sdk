use serde::{Deserialize, Serialize};
use sloper_extension::Resource;

#[derive(Deserialize, Serialize, Resource)]
#[resource(name = "invoices", key = id)]
struct Invoice { id: String }

fn main() {}
