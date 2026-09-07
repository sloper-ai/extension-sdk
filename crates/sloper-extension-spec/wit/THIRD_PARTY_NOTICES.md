# WASI interface provenance

The seven `wasi:*` definitions in `deps/` come unchanged from
[`wasip2` 2.0.0+wasi-0.2.12](https://crates.io/crates/wasip2/2.0.0+wasi-0.2.12),
at Bytecode Alliance's
[`wasi-rs` revision b0b9348](https://github.com/bytecodealliance/wasi-rs/tree/b0b9348b0335eb0d0c29ba1d27fcbd676ab079b0/crates/wasip2).

The crate uses `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`. Keep this
notice and the preserved [Apache](licenses/LICENSE-APACHE),
[Apache with LLVM exception](licenses/LICENSE-Apache-2.0_WITH_LLVM-exception),
and [MIT](licenses/LICENSE-MIT) licenses with redistributed definitions.

The WebAssembly Community Group maintains WASI. Its
[license notice](https://github.com/WebAssembly/wasi-cli/blob/main/LICENSE.md)
credits the Contributors to the WASI Specification and references the
[W3C Community Contributor License Agreement](https://www.w3.org/community/about/agreements/cla/).

| Definition | SHA-256 |
| --- | --- |
| `cli.wit` | `fb6ac5d23fcaf3d231142a3c2f9bb1e9bc1fe5af6f1f329f6b1a5555ce0d3873` |
| `clocks.wit` | `6ed8aa65bb8cbe224a0b2cbac9fc1b3bd25bdb17eda5ae0d23c983ed31c447cc` |
| `filesystem.wit` | `e675f261017bf9b4fa7df5b9701023fe0249ede71021ac1bf978748e62edcda8` |
| `http.wit` | `4bbd58f509700a6637385611f183c5bd5984d81feef85e29dfdc05ebd283045a` |
| `io.wit` | `96e206d00076fa0480df32c5bcf255a3fa4862805ac2f6b8537a781cce54f433` |
| `random.wit` | `48578c40213d5cab6650980905fa146a0be8c39c4433ee5e7e00637b87dbe08f` |
| `sockets.wit` | `ad38dbf3b0bbdf34c0f2b608edcbfee04d71b443a63faff1f5e0053c3f84b377` |

Sloper's `world.wit` and `deps/sloper-api.wit` use the Sloper Ecosystem License;
the WASI definitions retain their upstream terms. When updating interfaces,
preserve their notices and refresh these hashes from the source used for
generation.
