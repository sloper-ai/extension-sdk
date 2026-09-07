# Validate a component

Check a compiled component before running or publishing it:

```sh
sloper-extension validate path/to/extension.wasm --json
```

Validation checks the embedded manifest and WIT compatibility without executing
guest code. It accepts components from any language.

## Choose a check

| Command | What it checks |
| --- | --- |
| `build` | Compiles SDK declarations and writes a validated component |
| `check` | Compares `dist/extension.wasm` with a fresh SDK build |
| `validate` | Checks the existing component and its manifest |
| `run` | Validates, instantiates, and runs an action in the local host |
| `verify` | Checks a signed release against supplied trust roots and host admission |

Each component must contain one top-level `sloper:manifest` section with UTF-8
JSON. SDK build metadata is optional for publication. Instantiation checks
happen when the host admits the component; static validation alone cannot
establish that it will run.

## Read the result

The validator writes one JSON object to standard output:

- `valid`: whether the component passed.
- `findings`: validation findings.
- `manifest`: the exact embedded JSON text on success, or `null` on rejection.

Use that exact manifest text for hashes and signatures. Reserializing JSON can
change its bytes.

| Exit code | Meaning |
| --- | --- |
| `0` | Accepted |
| `2` | Invalid command usage |
| `3` | Rejected; inspect `findings` |
| `10` | Operational failure, such as an unreadable file |

The limits are 64 MiB per component and 256 KiB per manifest. Publication
applies the same static checks and returns HTTP 422 with findings for invalid
content. See [publishing](publishing.md) for authorization requirements.

## Use the Rust APIs

In `sloper-extension-host`, use `validate_component` for static validation and
`extract_manifest` for the embedded bytes. `check_component` also verifies SDK
declaration reassembly. Enable the `runtime` feature when you need execution; it
is off by default.

For files, `sloper_extension_cli::validate_file` returns a `Validation` result
for accepted or rejected components and a `ValidationError` for operational
failures.

The canonical interfaces live in
[sloper-extension-spec/wit](../crates/sloper-extension-spec/wit). Edit them
there and preserve the
[WASI notices](../crates/sloper-extension-spec/wit/THIRD_PARTY_NOTICES.md).
