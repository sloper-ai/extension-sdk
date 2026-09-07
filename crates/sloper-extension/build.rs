use std::{
    env,
    error::Error,
    path::PathBuf,
};

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is required by Cargo")?);
    sloper_extension_spec::write_wit(&output.join("wit"))?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
