# Host acceptance fixture

This guest tests Rust WASI imports and the Sloper world without depending on the
public SDK.

After changing fixture inputs, regenerate and test from the SDK root:

```sh
cargo run --locked \
  --manifest-path crates/sloper-extension-host/tests/guest/Cargo.toml \
  --bin build-fixture --target-dir target
cargo test --locked -p sloper-extension-host --features runtime --tests
```

The [generator](src/bin/build-fixture.rs) updates the component, input digest,
and CLI fixture copy. Use the
[pinned toolchain](../../../../rust-toolchain.toml); tests detect stale inputs
and embedded machine paths.
