use serde::{Deserialize, Serialize};
use sloper_extension::{action, Reader, Resource, Result, Writer};

#[derive(Deserialize, Serialize, Resource)]
#[resource(name = "invoices", key = id)]
struct Invoice {
    #[schema(max_length = 512)]
    id: String,
}

#[action]
async fn normalize(_input: Reader<Invoice>, _output: Writer<Invoice>) -> Result<()> {
    Ok(())
}

fn main() {
    assert!(__sloper_action_normalize::JSON.contains("[\"read\",\"write\"]"));
}
