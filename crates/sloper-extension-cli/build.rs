use std::{
    ffi::OsString,
    fs,
    io,
    path::Path,
    string::FromUtf8Error,
};

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0} must contain a full 40-character hexadecimal Git revision")]
    InvalidRevision(&'static str),
    #[error("Cargo package git.dirty must be a boolean")]
    InvalidDirtyFlag,
    #[error("Cargo package .cargo_vcs_info.json contains malformed JSON")]
    Metadata(#[from] serde_json::Error),
    #[error("SDK build provenance could not be read")]
    Io(#[from] io::Error),
    #[error("SDK checkout Git HEAD could not be resolved")]
    GitFailed,
    #[error("SDK checkout Git HEAD is not UTF-8")]
    GitEncoding(#[from] FromUtf8Error),
}

impl Error {
    fn invalid_revision(source: &'static str) -> Self {
        Self::InvalidRevision(source)
    }

    fn invalid_dirty_flag() -> Self {
        Self::InvalidDirtyFlag
    }

    fn git_failed() -> Self {
        Self::GitFailed
    }
}

#[cfg(not(test))]
fn main() -> Result<(), Error> {
    println!("cargo:rerun-if-env-changed=SLOPER_EXTENSION_SDK_REVISION");
    println!("cargo:rerun-if-changed=release-revision");
    println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
    println!("cargo:rerun-if-changed=../../.git");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide the package manifest directory");
    if let Some(revision) = resolve_revision(
        Path::new(&manifest),
        std::env::var_os("SLOPER_EXTENSION_SDK_REVISION"),
        git_head,
    )? {
        println!("cargo:rustc-env=SLOPER_EXTENSION_SDK_REVISION={revision}");
    }
    Ok(())
}

fn resolve_revision(
    manifest: &Path,
    explicit: Option<OsString>,
    git_head: impl FnOnce(&Path) -> Result<String, Error>,
) -> Result<Option<String>, Error> {
    if let Some(explicit) = explicit {
        return validated_revision(explicit.to_str(), "SLOPER_EXTENSION_SDK_REVISION").map(Some);
    }
    // Release automation stamps package versions without moving the source
    // commit. Carry that commit explicitly for later registry installations.
    match fs::read_to_string(manifest.join("release-revision")) {
        Ok(revision) => return validated_revision(Some(revision.trim()), "release-revision").map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    match fs::read(manifest.join(".cargo_vcs_info.json")) {
        Ok(bytes) => {
            let metadata: serde_json::Value = serde_json::from_slice(&bytes)?;
            let revision = validated_revision(
                metadata
                    .get("git")
                    .and_then(|git| git.get("sha1"))
                    .and_then(serde_json::Value::as_str),
                "Cargo package git.sha1",
            )?;
            return match metadata.get("git").and_then(|git| git.get("dirty")) {
                // An archive containing uncommitted changes cannot claim that
                // its contents came from the recorded commit. Keep the build
                // usable but require an explicit scaffold revision.
                Some(serde_json::Value::Bool(true)) => Ok(None),
                Some(serde_json::Value::Bool(false)) | None => Ok(Some(revision)),
                Some(_) => Err(Error::invalid_dirty_flag()),
            };
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }

    // Registry archives are not SDK workspace checkouts, even when unpacked
    // inside another Git repository. Never let Git discover an outer checkout.
    let Some(crates) = manifest
        .parent()
        .filter(|directory| directory.file_name().is_some_and(|name| name == "crates"))
    else {
        return Ok(None);
    };
    if manifest.file_name().is_none_or(|name| name != "sloper-extension-cli") {
        return Ok(None);
    }
    let Some(sdk_root) = crates.parent() else {
        return Ok(None);
    };
    if !sdk_root.join(".git").try_exists()? {
        return Ok(None);
    }
    validated_revision(Some(&git_head(sdk_root)?), "SDK checkout Git HEAD").map(Some)
}

fn validated_revision(value: Option<&str>, source: &'static str) -> Result<String, Error> {
    match value {
        Some(value) if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Ok(value.to_ascii_lowercase())
        },
        _ => Err(Error::invalid_revision(source)),
    }
}

#[cfg(not(test))]
fn git_head(sdk_root: &Path) -> Result<String, Error> {
    let output = std::process::Command::new("git")
        .current_dir(sdk_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err(Error::git_failed());
    }
    let watch = std::process::Command::new("git")
        .current_dir(sdk_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "rev-parse",
            "--git-path",
            "HEAD",
            "--git-path",
            "refs",
            "--git-path",
            "packed-refs",
        ])
        .output()?;
    if !watch.status.success() {
        return Err(Error::git_failed());
    }
    // In linked worktrees .git is a pointer file. Watch the resolved HEAD and
    // reference paths as well so a new checkout revision rebuilds the metadata.
    for path in String::from_utf8(watch.stdout)?.lines() {
        println!("cargo:rerun-if-changed={}", sdk_root.join(path).display());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "1234567890abcdef1234567890abcdef12345678";
    const EXPLICIT: &str = "abcdef1234567890abcdef1234567890abcdef12";

    fn no_git(_: &Path) -> Result<String, Error> {
        panic!("Git must not be queried for this provenance source")
    }

    #[test]
    fn package_archive_uses_cargo_revision_without_git() {
        let package = tempfile::tempdir().unwrap();
        fs::write(
            package.path().join(".cargo_vcs_info.json"),
            serde_json::json!({"git":{"sha1":REVISION},"path_in_vcs":"crates/sloper-extension-cli"}).to_string(),
        )
        .unwrap();
        assert_eq!(
            resolve_revision(package.path(), None, no_git).unwrap().as_deref(),
            Some(REVISION)
        );
    }

    #[test]
    fn stamped_package_keeps_original_source_revision() {
        let package = tempfile::tempdir().unwrap();
        fs::write(package.path().join("release-revision"), format!("{REVISION}\n")).unwrap();
        fs::write(
            package.path().join(".cargo_vcs_info.json"),
            serde_json::json!({"git":{"sha1":REVISION,"dirty":true}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            resolve_revision(package.path(), None, no_git).unwrap().as_deref(),
            Some(REVISION)
        );
        assert_eq!(
            resolve_revision(package.path(), Some(EXPLICIT.into()), no_git)
                .unwrap()
                .as_deref(),
            Some(EXPLICIT)
        );
    }

    #[test]
    fn malformed_release_revision_is_rejected() {
        let package = tempfile::tempdir().unwrap();
        for revision in ["", "main", "123456789abc", "not a commit"] {
            fs::write(package.path().join("release-revision"), revision).unwrap();
            assert!(matches!(
                resolve_revision(package.path(), None, no_git),
                Err(Error::InvalidRevision("release-revision"))
            ));
        }
    }

    #[test]
    fn dirty_package_has_no_default_or_checkout_fallback() {
        let sdk = tempfile::tempdir().unwrap();
        let manifest = sdk.path().join("crates/sloper-extension-cli");
        fs::create_dir_all(&manifest).unwrap();
        fs::create_dir(sdk.path().join(".git")).unwrap();
        fs::write(
            manifest.join(".cargo_vcs_info.json"),
            serde_json::json!({"git":{"sha1":REVISION,"dirty":true}}).to_string(),
        )
        .unwrap();
        assert_eq!(resolve_revision(&manifest, None, no_git).unwrap(), None);
    }

    #[test]
    fn explicit_revision_overrides_dirty_package_provenance() {
        let package = tempfile::tempdir().unwrap();
        fs::write(
            package.path().join(".cargo_vcs_info.json"),
            serde_json::json!({"git":{"sha1":REVISION,"dirty":true}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            resolve_revision(package.path(), Some(EXPLICIT.into()), no_git)
                .unwrap()
                .as_deref(),
            Some(EXPLICIT),
        );
    }

    #[test]
    fn package_dirty_flag_requires_a_boolean() {
        let package = tempfile::tempdir().unwrap();
        for dirty in [serde_json::json!("true"), serde_json::json!(1), serde_json::Value::Null] {
            fs::write(
                package.path().join(".cargo_vcs_info.json"),
                serde_json::json!({"git":{"sha1":REVISION,"dirty":dirty}}).to_string(),
            )
            .unwrap();
            assert!(matches!(
                resolve_revision(package.path(), None, no_git),
                Err(Error::InvalidDirtyFlag)
            ));
        }
    }

    #[test]
    fn explicit_revision_overrides_even_malformed_package_metadata() {
        let package = tempfile::tempdir().unwrap();
        fs::write(package.path().join(".cargo_vcs_info.json"), b"not JSON").unwrap();
        assert_eq!(
            resolve_revision(package.path(), Some(EXPLICIT.into()), no_git)
                .unwrap()
                .as_deref(),
            Some(EXPLICIT)
        );
    }

    #[test]
    fn invalid_explicit_revision_is_not_replaced_by_other_provenance() {
        let package = tempfile::tempdir().unwrap();
        fs::write(
            package.path().join(".cargo_vcs_info.json"),
            serde_json::json!({"git":{"sha1":REVISION}}).to_string(),
        )
        .unwrap();
        for explicit in [
            "",
            "main",
            "1234567",
            "1234567890abcdef1234567890abcdef1234567g",
            "1234567890abcdef1234567890abcdef12345678\n",
        ] {
            assert!(resolve_revision(package.path(), Some(explicit.into()), no_git).is_err());
        }
    }

    #[test]
    fn malformed_package_metadata_is_reported_without_git_fallback() {
        let package = tempfile::tempdir().unwrap();
        for metadata in [
            "not JSON",
            "{}",
            r#"{"git":{"sha1":3}}"#,
            r#"{"git":{"sha1":"main"}}"#,
            r#"{"git":{"sha1":"main","dirty":true}}"#,
        ] {
            fs::write(package.path().join(".cargo_vcs_info.json"), metadata).unwrap();
            assert!(resolve_revision(package.path(), None, no_git).is_err());
        }
    }

    #[test]
    fn unreadable_package_metadata_is_reported() {
        let package = tempfile::tempdir().unwrap();
        fs::create_dir(package.path().join(".cargo_vcs_info.json")).unwrap();
        assert!(resolve_revision(package.path(), None, no_git).is_err());
    }

    #[test]
    fn checkout_requires_its_own_git_marker() {
        let parent = tempfile::tempdir().unwrap();
        fs::create_dir(parent.path().join(".git")).unwrap();
        let sdk = parent.path().join("extension-sdk");
        let manifest = sdk.join("crates/sloper-extension-cli");
        fs::create_dir_all(&manifest).unwrap();
        assert_eq!(resolve_revision(&manifest, None, no_git).unwrap(), None);
        fs::write(sdk.join(".git"), "gitdir: linked-worktree").unwrap();
        assert_eq!(
            resolve_revision(&manifest, None, |root| {
                assert_eq!(root, sdk);
                Ok(REVISION.into())
            })
            .unwrap()
            .as_deref(),
            Some(REVISION)
        );
    }

    #[test]
    fn package_directory_cannot_borrow_unrelated_repository_head() {
        let parent = tempfile::tempdir().unwrap();
        fs::create_dir(parent.path().join(".git")).unwrap();
        let manifest = parent
            .path()
            .join(format!("packages/sloper-extension-cli-{}", env!("CARGO_PKG_VERSION")));
        fs::create_dir_all(&manifest).unwrap();
        assert_eq!(resolve_revision(&manifest, None, no_git).unwrap(), None);
    }

    #[test]
    fn checkout_git_failures_are_reported() {
        let sdk = tempfile::tempdir().unwrap();
        let manifest = sdk.path().join("crates/sloper-extension-cli");
        fs::create_dir_all(&manifest).unwrap();
        fs::create_dir(sdk.path().join(".git")).unwrap();
        assert!(resolve_revision(&manifest, None, |_| Err(Error::git_failed())).is_err());
    }

    #[test]
    fn git_revision_is_validated_before_embedding() {
        let sdk = tempfile::tempdir().unwrap();
        let manifest = sdk.path().join("crates/sloper-extension-cli");
        fs::create_dir_all(&manifest).unwrap();
        fs::create_dir(sdk.path().join(".git")).unwrap();
        assert!(resolve_revision(&manifest, None, |_| Ok("main".into())).is_err());
        assert_eq!(
            resolve_revision(&manifest, None, |_| Ok(REVISION.to_uppercase()))
                .unwrap()
                .as_deref(),
            Some(REVISION)
        );
    }
}
