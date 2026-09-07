# SDK execution fixture

This guest tests the public `sloper-extension` API through the host.

After changing the guest, SDK, macros, lockfiles, or WIT, regenerate and test
from the SDK root:

```sh
cargo run --locked \
  --manifest-path crates/sloper-extension-host/tests/sdk-guest/Cargo.toml \
  --bin build-sdk-fixture --target-dir target
cargo test --locked -p sloper-extension-host --features runtime --test sdk_execution
```

Use the [pinned toolchain](../../../../rust-toolchain.toml). The
[generator](src/bin/build-sdk-fixture.rs) records fixture inputs; tests detect
stale inputs and embedded machine paths.
