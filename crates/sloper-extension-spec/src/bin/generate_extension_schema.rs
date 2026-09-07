//! Emits the public extension manifest JSON Schema.

use std::{
    error::Error,
    io::{
        self,
        Write,
    },
};

use sloper_extension_spec::Manifest;

fn main() -> Result<(), Box<dyn Error>> {
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &Manifest::json_schema())?;
    output.write_all(b"\n")?;
    Ok(())
}
