use std::{
    env,
    error::Error,
    fs,
    path::{
        Component,
        Path,
        PathBuf,
    },
};

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is required by Cargo")?);
    let wit = match env::var_os("SLOPER_EXTENSION_HOST_WIT_DIR") {
        // A build that already holds the WIT tree names it relative to the
        // package directory. Bazel does: `bindgen!` runs again when the doc
        // tests run, in a runfiles tree where OUT_DIR is laid out differently
        // than where this script wrote it, while the spec crate's own `wit/`
        // sits next to this package in both trees.
        Some(directory) => PathBuf::from(directory),
        None => {
            let wit = output.join("wit");
            sloper_extension_spec::write_wit(&wit)?;
            // An absolute OUT_DIR only exists where this script ran, and Bazel
            // runs rustc in another sandbox. `bindgen!` resolves a relative
            // path against CARGO_MANIFEST_DIR, the package directory build
            // scripts run in, so a path relative to the working directory
            // holds in both places.
            relative_to(&wit, &env::current_dir()?).unwrap_or(wit)
        },
    };
    let template = fs::read_to_string("src/bindings.rs.in")?;
    let quoted = format!("{:?}", wit.to_str().ok_or("WIT build path must be valid UTF-8")?);
    fs::write(
        output.join("bindings.rs"),
        template.replace("\"WIT_DIRECTORY\"", &quoted),
    )?;
    println!("cargo:rerun-if-changed=src/bindings.rs.in");
    println!("cargo:rerun-if-env-changed=SLOPER_EXTENSION_HOST_WIT_DIR");
    Ok(())
}

fn relative_to(target: &Path, base: &Path) -> Option<PathBuf> {
    let target: Vec<Component<'_>> = target.components().collect();
    let base: Vec<Component<'_>> = base.components().collect();
    let common = target.iter().zip(&base).take_while(|(a, b)| a == b).count();
    // Paths on different roots, such as two Windows drives, have no relative form.
    if common == 0 {
        return None;
    }
    let mut relative = PathBuf::new();
    for _ in common..base.len() {
        relative.push("..");
    }
    for component in &target[common..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}
