#![forbid(unsafe_code)]

use sloper_extension::{Result, action};

#[action]
async fn run() -> Result<()> {
    Ok(())
}

fn main() {
    assert!(__sloper_action_run::JSON.contains("run"));
}
