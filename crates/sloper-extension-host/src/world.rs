use std::{
    collections::BTreeSet,
    fmt::Write as _,
};

use semver::Version;
use sloper_extension_spec::Manifest;
use wit_component::DecodedWasm;
use wit_parser::{
    Resolve,
    UnresolvedPackageGroup,
    WorldId,
    WorldItem,
    WorldKey,
};

use crate::{
    ComponentError,
    extract_manifest,
};

// The same vendored packages generate the executable host's linker bindings.
const PACKAGES: &[(&str, &str)] = sloper_extension_spec::WIT_PACKAGES;

/// Validates a component's embedded document and complete declared world.
///
/// The pinned component parser validates executable bytes and static world
/// subtyping without instantiating or running guest code. Runtime linker
/// admission remains a separate check against the configured host.
///
/// # Errors
/// Rejects malformed components, invalid manifests, unsupported imports or
/// versions, and exports that differ from the asynchronous Sloper action.
///
/// ```
/// use sloper_extension_host::validate_component;
/// assert!(validate_component(b"invalid").is_err());
/// ```
pub fn validate_component(component: &[u8]) -> Result<Manifest, ComponentError> {
    let manifest = Manifest::parse(extract_manifest(component)?)?;
    let (actual, actual_world) = match wit_component::decode(component)
        .map_err(|source| ComponentError::world(source.into_boxed_dyn_error()))?
    {
        DecodedWasm::Component(resolve, world) => (resolve, world),
        DecodedWasm::WitPackage(_, _) => {
            return Err(invalid_world("Expected an executable component, not a WIT package."));
        },
    };
    let (mut expected, world) = host_world()?;
    validate_exports(&actual, actual_world, &expected, world)?;
    match_import_names(&actual, actual_world, &mut expected, world)?;
    // `targets` creates a type-checking wrapper and validates its syntax and
    // structural subtyping with component-model async enabled. It runs no VM.
    wit_component::targets(&expected, world, component)
        .map_err(|source| ComponentError::world(source.into_boxed_dyn_error()))?;
    Ok(manifest)
}

fn validate_exports(
    actual: &Resolve,
    actual_world: WorldId,
    expected: &Resolve,
    expected_world: WorldId,
) -> Result<(), ComponentError> {
    let actual_exports = actual.worlds[actual_world]
        .exports
        .keys()
        .map(|key| actual.name_world_key(key))
        .collect::<BTreeSet<_>>();
    let expected_exports = expected.worlds[expected_world]
        .exports
        .keys()
        .map(|key| expected.name_world_key(key))
        .collect::<BTreeSet<_>>();
    if actual_exports != expected_exports {
        return Err(invalid_world(
            "Component must export exactly the Sloper action interface.",
        ));
    }
    for item in actual.worlds[actual_world].exports.values() {
        let WorldItem::Interface {
            id, ..
        } = item
        else {
            return Err(invalid_world("The action export must be an interface."));
        };
        let functions = &actual.interfaces[*id].functions;
        if functions.len() != 1 || !functions.contains_key("run") {
            return Err(invalid_world("The action interface must export only run."));
        }
    }
    Ok(())
}

fn match_import_names(
    actual: &Resolve,
    actual_world: WorldId,
    expected: &mut Resolve,
    expected_world: WorldId,
) -> Result<(), ComponentError> {
    let supported = expected.worlds[expected_world]
        .imports
        .keys()
        .map(|key| expected.name_world_key(key))
        .collect::<BTreeSet<_>>();
    let mut versions = BTreeSet::new();
    for key in actual.worlds[actual_world].imports.keys() {
        let name = actual.name_world_key(key);
        if supported.contains(&name) {
            continue;
        }
        let Some((interface, version)) = name.rsplit_once('@') else {
            return Err(invalid_world(
                "Private or unversioned component imports are not supported.",
            ));
        };
        let version = Version::parse(version).map_err(|source| ComponentError::world(Box::new(source)))?;
        if !interface.starts_with("wasi:")
            || version.major != 0
            || version.minor != 2
            || !version.pre.is_empty()
            || !version.build.is_empty()
        {
            return Err(invalid_world("Component imports an unsupported package or version."));
        }
        if !supported.contains(&format!("{interface}@0.2.12")) {
            return Err(invalid_world("Component imports an unsupported WASI interface."));
        }
        versions.insert(version);
    }
    for version in versions {
        let imports = compatible_wasi_imports(expected, &version)?;
        for (key, item) in imports {
            expected.worlds[expected_world].imports.insert(key, item);
        }
    }
    Ok(())
}

fn compatible_wasi_imports(
    resolve: &mut Resolve,
    version: &Version,
) -> Result<Vec<(WorldKey, WorldItem)>, ComponentError> {
    let packages = PACKAGES
        .iter()
        .filter(|(name, _)| *name != "sloper-api.wit")
        .map(|(name, source)| {
            // The host's stable 0.2.12 types are offered under a semver-compatible
            // import version. WIT release annotations are publication metadata,
            // not part of these interface types. Keep feature gates intact.
            let source = compatible_wasi_source(source, version);
            parse(name, &source)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let source = format!(
        "package sloper:admission@{version}; world host {{ include wasi:cli/imports@{version}; import \
         wasi:http/types@{version}; import wasi:http/outgoing-handler@{version}; }}"
    );
    let package = resolve
        .push_groups(parse("admission.wit", &source)?, packages)
        .map_err(|source| ComponentError::world(Box::new(source)))?;
    let world = resolve
        .select_world(&[package], Some("host"))
        .map_err(|source| ComponentError::world(source.into_boxed_dyn_error()))?;
    Ok(resolve.worlds[world]
        .imports
        .iter()
        .map(|(key, item)| (key.clone(), item.clone()))
        .collect())
}

fn compatible_wasi_source(source: &str, version: &Version) -> String {
    let mut output = String::with_capacity(source.len());
    let mut lines = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("@since(") && !line.trim_start().starts_with("@deprecated("))
        .peekable();
    while let Some(line) = lines.next() {
        let mut rest = line;
        while let Some((before, after)) = rest.split_once("@0.2.12") {
            output.push_str(before);
            write!(output, "@{version}").expect("writing to a String cannot fail");
            rest = after;
        }
        output.push_str(rest);
        if lines.peek().is_some() {
            output.push('\n');
        }
    }
    output
}

fn host_world() -> Result<(Resolve, WorldId), ComponentError> {
    let mut resolve = Resolve::default();
    let packages = PACKAGES
        .iter()
        .map(|(name, source)| parse(name, source))
        .collect::<Result<Vec<_>, _>>()?;
    let package = resolve
        .push_groups(parse("extension.wit", sloper_extension_spec::WIT_WORLD)?, packages)
        .map_err(|source| ComponentError::world(Box::new(source)))?;
    let world = resolve
        .select_world(&[package], Some("extension"))
        .map_err(|source| ComponentError::world(source.into_boxed_dyn_error()))?;
    Ok((resolve, world))
}

fn parse(name: &str, source: &str) -> Result<UnresolvedPackageGroup, ComponentError> {
    UnresolvedPackageGroup::parse(name, source).map_err(|(_, source)| ComponentError::world(Box::new(source)))
}

fn invalid_world(message: &str) -> ComponentError {
    ComponentError::invalid("MANIFEST_WORLD_INVALID", message)
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::{
        PACKAGES,
        compatible_wasi_source,
    };

    #[test]
    fn compatible_source_preserves_filtered_bytes_and_literal_replacements() {
        for version in ["0.2.0", "0.2.9", "0.2.12", "0.2.100"] {
            let version = Version::parse(version).unwrap();
            for source in PACKAGES.iter().map(|(_, source)| *source).chain([
                "",
                "\n",
                "@since(version = 0.2.1)\n",
                "@0.2.12 @0.2.12\n\n @deprecated(version = 0.2.2)\nlast\n",
            ]) {
                let expected = source
                    .lines()
                    .filter(|line| {
                        !line.trim_start().starts_with("@since(") && !line.trim_start().starts_with("@deprecated(")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .replace("@0.2.12", &format!("@{version}"));
                assert_eq!(compatible_wasi_source(source, &version), expected);
            }
        }
    }
}
