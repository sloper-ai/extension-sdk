# Release the SDK

Push the selected source commit to the `release` branch. The
[release workflow](../.github/workflows/release.yml) automatically publishes the
six SDK crates and native CLI archives from that exact commit. Moving the branch
selects the whole snapshot, including its history; it does not cherry-pick changes.

```sh
git push origin CHOSEN_SHA:refs/heads/release
```

The selected commit must contain the release workflow and its scripts. A normal
push advances the branch. To select an older or divergent commit, explicitly use
`--force-with-lease=refs/heads/release:EXPECTED_REMOTE_SHA` after checking the current
remote revision. Configure branch rules to allow the intended release maintainers
to make these updates.

## Version identity

Every published crate uses `0.0.0-YYYYMMDDHHMMSS.gSHA`, for example
`0.0.0-20260908090304.g800a6ef6e39d`. The date is the source commit's **committer
timestamp in UTC**, and `SHA` is its first 12 hexadecimal characters. The GitHub tag
adds a `v` prefix. The full commit is recorded in `release.json`, shipped with each
archive and attached to the GitHub release.

The same commit always produces the same version, including on retries. Versions
identify snapshots and carry no compatibility promise. This uses Cargo's
prerelease version syntax; consumers should request the exact published version:

```sh
cargo install sloper-extension-cli --version '=0.0.0-20260908090304.g800a6ef6e39d' --locked
```

The example version illustrates the format; substitute a version from the releases
page. Development manifests keep their checked-in versions. CI uses
`cargo-workspaces` to stamp all publishable crates and pin their internal
dependencies with `=`. It creates no version commit, version PR, or additional
branch update. Examples and fixture crates remain unpublished.

## Build and publish

The workflow has three stages:

1. Check out the triggering event's immutable SHA and run repository checks. Derive
   its version, stamp the manifests, regenerate fixtures, build and test the
   workspace, and check package contents. Save the prepared source for the
   remaining jobs.
2. Build the native CLI archives and checksums with
   `taiki-e/upload-rust-binary-action`. Every target uses the same prepared source.
3. Create or reuse a GitHub draft and upload its assets. Publish the crates in
   dependency order with `cargo workspaces publish`, then publish the GitHub release
   only after every crate succeeds.

The [Nushell release script](../.github/scripts/release.nu) owns version identity,
source preparation and release state checks. The workflow owns the target matrix
and publication credentials. Regular source verification remains in
[verify.yml](../.github/workflows/verify.yml).

Verification and release preparation use `mise run //:check`,
`mise run '//...:build'`, and `mise run '//...:test'`. The `//:package` task owns
Cargo packaging and is shared by repository checks and release preparation.

The root mise tasks own workspace builds and tests, so host/spec tests run once
per configuration. Cargo uses the runner's available CPU parallelism. Native CLI
archives use the release profile with optimization level 3, ThinLTO and stripped
symbols. Development builds and tests keep their debug assertions and checks.

Native binaries and source-installed CLIs both retain the original source commit
for scaffolding SDK dependencies. CI includes a `release-revision` file in the CLI
crate because version stamping makes Cargo's ordinary Git metadata dirty.

Mise caches the pinned tools and Rust toolchain together. Its key includes the
mise configuration, platform and Rust toolchain file. Cargo caches are restored
before fetching dependencies or applying release versions. Verification and
release preparation share the Ubuntu dependency cache; publication restores that
cache without writing it. Native builds have separate caches for each target and
runner environment. Cache keys also include the root manifest's profile settings,
Rust compiler, compiler flags and dependency manifests. PRs restore caches; only `main`
and `release` runs save them. Release archives remain workflow artifacts.

## Dry runs and retries

Manual dispatch defaults to a dry run. Select a branch or tag containing the
workflow to build and verify its snapshot without publishing:

```sh
gh workflow run release.yml --ref main --field dry_run=true
```

Download the `release-source` and `binaries-*` workflow artifacts to inspect the
prepared source, version metadata, archives and checksums. Dry runs create no
remote tag, GitHub release or registry version.

To publish the current release branch manually:

```sh
gh workflow run release.yml --ref release --field dry_run=false
```

For a failed release, rerun failed jobs. This retains the original commit and
successful build artifacts even if the release branch has moved:

```sh
gh run rerun RUN_ID --failed
```

Retries reuse a draft, skip crate versions already present in the registry, and
leave completed releases untouched. A tag or draft pointing at a different commit
fails the run. API errors also fail the run instead of being treated as a missing
release. Crates.io publication is not atomic: a failure may leave some crates
published while the GitHub release stays a draft. Retry the same run to complete it.
Changed source requires a new selected commit.

## Publishing setup

Create the repository's `release` environment and permit publication from its
`release` branch. Configure
[crates.io trusted publishing](https://crates.io/docs/trusted-publishing) for each
of the six packages with owner `sloper-ai`, repository `extension-sdk`, workflow
`release.yml`, and environment `release`. The workflow obtains a short-lived
registry token and uses `GITHUB_TOKEN` for the GitHub release. Environment approval
rules, if configured, still apply.

If a crate has never been published, its owner must publish it once before setting
up trusted publishing. From a clean checkout of the selected commit, use the
repository's Nushell environment to prepare the same versions and fixtures, then
publish using Cargo's credential provider:

```nu
nu .github/scripts/release.nu prepare --dry-run
cargo workspaces publish --publish-as-is --allow-dirty --locked --no-remove-dev-deps
```

After trusted publishing is configured, rerun the workflow at that commit. It
skips the existing crate versions and completes the GitHub release.

## CLI archives

| Platform | Target | Format |
| --- | --- | --- |
| macOS Apple silicon | `aarch64-apple-darwin` | `.tar.gz` |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| Windows x86-64 | `x86_64-pc-windows-msvc` | `.zip` |

Linux builds use GitHub's Ubuntu 24.04 runners. Archive names are `sloper-extension-cli-TARGET`
with the format suffix; each has a `sloper-extension-cli-TARGET.sha256` checksum
file. Archives contain the CLI, `LICENSE`, `ThirdPartyNotices.txt` and
`release.json` at the root. The CLI crate's binstall metadata matches this layout.
See [licensing](licensing.md) for the dependency report's scope.
