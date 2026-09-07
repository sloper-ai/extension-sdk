use sloper_extension::{Result, action};

#[action]
async unsafe fn unsafe_action() -> Result<()> {
    Ok(())
}

fn main() {}
