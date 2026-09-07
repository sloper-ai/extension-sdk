use std::{
    env,
    error::Error,
    fs,
    path::PathBuf,
};

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is required by Cargo")?);
    let wit = output.join("wit");
    sloper_extension_spec::write_wit(&wit)?;
    let template = fs::read_to_string("src/bindings.rs.in")?;
    let quoted = format!("{:?}", wit.to_str().ok_or("WIT build path must be valid UTF-8")?);
    fs::write(
        output.join("bindings.rs"),
        template.replace("\"WIT_DIRECTORY\"", &quoted),
    )?;
    println!("cargo:rerun-if-changed=src/bindings.rs.in");
    Ok(())
}
