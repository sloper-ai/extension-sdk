# Sloper extension SDK

Build Rust actions that call provider APIs and work with Sloper resources. The
SDK turns your declarations into WebAssembly components; the `sloper-extension`
CLI builds, runs, and publishes them.

## Get started

Choose a version from the [releases page](https://github.com/sloper-ai/extension-sdk/releases)
and replace `VERSION` below, omitting the tag's leading `v`:

```sh
cargo binstall sloper-extension-cli --version VERSION --no-confirm --disable-strategies compile,quick-install
```

With Rust installed through rustup, create an extension and run its sample action:

```sh
sloper-extension new acme.records
cd acme.records
sloper-extension build
sloper-extension run normalize --resource records=records.jsonl --out run
```

The scaffold includes a Rust toolchain file, a pinned SDK revision, and sample
records. The sample action trims record labels and writes its results to
`run/records.jsonl`.

## Guides

- [Write an extension](docs/authoring.md): actions, schemas, resources, and
  provider connections.
- [Validate a component](docs/validation.md): CLI checks, JSON results, and host
  APIs.
- [Publish an extension](docs/publishing.md): tokens, visibility, and versions.
- [Release the SDK](docs/releases.md): maintainer setup and CLI archives.

See [echo](examples/echo/src/lib.rs) for a simple action or
[sync](examples/sync/src/lib.rs) for reading and writing resources.

## Develop the SDK

From a checkout, build the CLI and try the echo example:

```sh
cargo build --locked -p sloper-extension-cli
./target/debug/sloper-extension build examples/echo
./target/debug/sloper-extension validate examples/echo/dist/extension.wasm
```

Install [mise](https://mise.jdx.dev/getting-started.html), then run:

```sh
mise run bootstrap
hk check --all --slow
mise run '//...:build'
mise run '//...:test'
```

Lint and formatter settings live in `pyproject.toml`. Tool versions and tasks are in `mise.toml`, and Git hook definitions are in `hk.pkl`.

Bootstrap installs the pinned tools, resolves locked dependencies, and installs
Git hooks through mise. Pre-commit formats staged files; commit messages must
follow Conventional Commits. Pre-push runs all checks, then builds and tests
affected projects. Use `hk fix --all` to apply available fixes. `mise run check` runs the same full check suite.

| Crate | Purpose |
| --- | --- |
| `sloper-extension` | Guest API for extension actions |
| `sloper-extension-macros` | Action and schema macros |
| `sloper-extension-spec` | Manifest types, schemas, and WIT |
| `sloper-extension-api` | Generated publication API client |
| `sloper-extension-host` | Component validation and execution |
| `sloper-extension-cli` | CLI and reusable authoring functions |

Sloper-owned code uses the [Sloper Ecosystem License](LICENSE). See
[licensing](docs/licensing.md) for permitted use and third-party notices.
