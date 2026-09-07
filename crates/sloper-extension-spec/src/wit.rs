//! Canonical WIT resources shared by static validators and binding generators.

use std::{
    fs,
    path::Path,
};

use crate::Error;

/// The Sloper extension world definition.
pub const WIT_WORLD: &str = include_str!("../wit/world.wit");

/// The packages imported by the extension world, with stable filenames.
pub const WIT_PACKAGES: &[(&str, &str)] = &[
    ("cli.wit", include_str!("../wit/deps/cli.wit")),
    ("clocks.wit", include_str!("../wit/deps/clocks.wit")),
    ("filesystem.wit", include_str!("../wit/deps/filesystem.wit")),
    ("http.wit", include_str!("../wit/deps/http.wit")),
    ("io.wit", include_str!("../wit/deps/io.wit")),
    ("random.wit", include_str!("../wit/deps/random.wit")),
    ("sloper-api.wit", include_str!("../wit/deps/sloper-api.wit")),
    ("sockets.wit", include_str!("../wit/deps/sockets.wit")),
];

/// Materializes canonical WIT files for a binding generator in a build
/// directory.
///
/// Existing canonical files are replaced so rebuilds cannot retain stale
/// inputs.
///
/// # Errors
/// Returns the underlying filesystem error if the destination cannot be
/// written.
///
/// ```no_run
/// sloper_extension_spec::write_wit(std::path::Path::new("target/wit"))?;
/// # Ok::<(), sloper_extension_spec::Error>(())
/// ```
pub fn write_wit(directory: &Path) -> Result<(), Error> {
    fs::create_dir_all(directory.join("deps"))?;
    fs::write(directory.join("world.wit"), WIT_WORLD)?;
    for (name, source) in WIT_PACKAGES {
        fs::write(directory.join("deps").join(name), source)?;
    }
    Ok(())
}
