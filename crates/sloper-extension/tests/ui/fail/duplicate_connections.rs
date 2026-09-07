use sloper_extension::{action, Connection, Result};

#[derive(Connection)]
#[connection(name = "ledger", profile = "acme.ledger", scopes = ["items.read"])]
struct Ledger;

#[derive(Connection)]
#[connection(name = "ledger", profile = "acme.other", scopes = ["items.read"])]
struct Other;

#[action]
async fn echo(_one: Connection<Ledger>, _two: Connection<Other>) -> Result<()> { Ok(()) }

fn main() {}
