use std::{
    fs,
    io::{
        self,
        Write as _,
    },
    path::{
        Path,
        PathBuf,
    },
    process::Stdio,
};

use serde::Deserialize;
use tokio::process::Command;

use super::super::Error;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    license_file: Option<PathBuf>,
    manifest_path: PathBuf,
}

pub(super) async fn copy(directory: &Path, dist: &Path, package_id: &str) -> Result<(), Error> {
    let mut command = Command::new("cargo");
    command
        .current_dir(directory)
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    super::strip_credentials(&mut command, std::env::vars_os().map(|(name, _)| name));
    let output = command.output().await?;
    if !output.status.success() {
        return Err(Error::invalid("Cargo package metadata failed; see standard error"));
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let package = metadata
        .packages
        .iter()
        .find(|package| package.id == package_id)
        .ok_or_else(|| Error::invalid_response("Built extension is absent from Cargo metadata"))?;
    copy_files(package, dist)
}

fn copy_files(package: &Package, dist: &Path) -> Result<(), Error> {
    let directory = package
        .manifest_path
        .parent()
        .ok_or_else(|| Error::invalid_response("Cargo package manifest has no directory"))?;
    // Cargo resolves workspace inheritance in license_file. An undeclared
    // license is taken only from this package, never an unrelated parent.
    let license = match &package.license_file {
        Some(path) => Some(fs::read(directory.join(path))?),
        None => read_optional(&directory.join("LICENSE"))?,
    };
    let third_party = read_optional(&directory.join("ThirdPartyNotices.txt"))?;
    // Read both inputs before changing the existing distribution.
    for (name, bytes) in [("LICENSE", license), ("ThirdPartyNotices.txt", third_party)] {
        let destination = dist.join(name);
        if let Some(bytes) = bytes {
            let mut temporary = tempfile::NamedTempFile::new_in(dist)?;
            temporary.write_all(&bytes)?;
            temporary.persist(destination).map_err(|error| error.error)?;
        } else if let Err(error) = fs::remove_file(destination)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
    }
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, io::Error> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(directory: &Path, license_file: Option<&str>) -> Package {
        Package {
            id: "extension".into(),
            license_file: license_file.map(PathBuf::from),
            manifest_path: directory.join("Cargo.toml"),
        }
    }

    #[test]
    fn inherited_license_and_authored_report_preserve_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("extension");
        let dist = directory.join("dist");
        fs::create_dir_all(&dist).unwrap();
        fs::write(root.path().join("LICENSE"), b"workspace terms\r\n").unwrap();
        fs::write(directory.join("ThirdPartyNotices.txt"), b"upstream attribution\r\n").unwrap();
        fs::write(dist.join("extension.wasm"), b"tested component").unwrap();

        copy_files(&package(&directory, Some("../LICENSE")), &dist).unwrap();

        assert_eq!(fs::read(dist.join("LICENSE")).unwrap(), b"workspace terms\r\n");
        assert_eq!(
            fs::read(dist.join("ThirdPartyNotices.txt")).unwrap(),
            b"upstream attribution\r\n"
        );
        assert_eq!(fs::read(dist.join("extension.wasm")).unwrap(), b"tested component");
    }

    #[test]
    fn local_license_is_used_without_inheriting_unrelated_parent_terms() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("extension");
        let dist = directory.join("dist");
        fs::create_dir_all(&dist).unwrap();
        fs::write(root.path().join("LICENSE"), b"unrelated terms").unwrap();
        fs::write(directory.join("LICENSE"), b"extension terms").unwrap();

        copy_files(&package(&directory, None), &dist).unwrap();
        assert_eq!(fs::read(dist.join("LICENSE")).unwrap(), b"extension terms");

        fs::remove_file(directory.join("LICENSE")).unwrap();
        copy_files(&package(&directory, None), &dist).unwrap();
        assert!(!dist.join("LICENSE").exists());
    }

    #[test]
    fn rebuilding_removes_report_when_author_removes_it() {
        let root = tempfile::tempdir().unwrap();
        let dist = root.path().join("dist");
        fs::create_dir(&dist).unwrap();
        let report = root.path().join("ThirdPartyNotices.txt");
        fs::write(&report, b"previous dependencies").unwrap();
        copy_files(&package(root.path(), None), &dist).unwrap();
        assert!(dist.join("ThirdPartyNotices.txt").exists());

        fs::remove_file(report).unwrap();
        copy_files(&package(root.path(), None), &dist).unwrap();
        assert!(!dist.join("ThirdPartyNotices.txt").exists());
    }

    #[test]
    fn missing_declared_license_preserves_existing_distribution() {
        let root = tempfile::tempdir().unwrap();
        let dist = root.path().join("dist");
        fs::create_dir(&dist).unwrap();
        fs::write(dist.join("LICENSE"), b"previous terms").unwrap();
        fs::write(dist.join("ThirdPartyNotices.txt"), b"previous report").unwrap();

        assert!(copy_files(&package(root.path(), Some("missing-license")), &dist).is_err());
        assert_eq!(fs::read(dist.join("LICENSE")).unwrap(), b"previous terms");
        assert_eq!(
            fs::read(dist.join("ThirdPartyNotices.txt")).unwrap(),
            b"previous report"
        );
    }
}
