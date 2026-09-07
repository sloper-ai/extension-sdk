# Write an extension

Create a project with the CLI:

```sh
sloper-extension new acme.records
cd acme.records
```

The scaffold includes a sample action and pins the SDK to a Git commit. Commit
the generated toolchain, SDK revision, and Cargo lockfile. Use `--sdk-revision`
with a full commit SHA when you need a different revision or your CLI build has
none.

## Declare actions

Use `extension!` to list your actions and `#[action]` to describe each function.
Cargo supplies the extension's version and description; action doc comments
supply action descriptions. The build generates the manifest from these
declarations.

| Declaration | Describes |
| --- | --- |
| `extension!` | Name, optional label and configuration, and actions |
| `#[action]` | Parameters, connections, and resource access |
| `Schema` | A value's JSON schema |
| `Connection` | A provider connection type |
| `Resource` | A resource, its key, and its schema |

An action can accept connections, readers, writers, and one owned parameters
value. Reader actions cannot also take owned parameters. Actions with a writer
allow at most one connection.

Use up to eight distinct resources per action. A resource can have one reader
and one writer; duplicate readers or writers are rejected. Actions with resource
access run in the background.

See the [echo action](../examples/echo/src/lib.rs) and
[resource sync](../examples/sync/src/lib.rs) examples.

## Define schemas

Use strings, booleans, finite floats, supported integers, options, arrays,
vectors, named structs, and unit enums. Bound resource keys. Supported string
formats are `date`, `date-time`, `email`, `uri`, and `uuid`.

`BTreeMap<String, T>` allows typed extra properties. Serde flatten on `Fields`
opens an object. Configuration must stay closed, and source fields belong only
in parameters or resource items.

The derives support serde `rename`, `rename_all`, `default`, `skip`, and flatten
on `Fields`. They reject recursive types, data-carrying enums, nested options,
`u64`, and `PathBuf`. Spell out containers or use a newtype when a type alias
hides their shape.

## Read and write resources

Readers fetch pages on demand. Each `Scanned<T>` dereferences to its item and
provides `seed()` and `seed_key(purpose)` for revision-based idempotency keys.
Use these keys to deduplicate provider requests on retries.

Writers accept `Batch<T>` values with items and an optional provider cursor.
Large batches may span transactions; the cursor advances after the final chunk.
An empty batch can advance the cursor. Reader actions have no provider cursor.

Open an existing source with `Source::open`. To produce one, use a writer's
declared source property and a filename, then close the upload before writing
its identity into an item. Failed or oversized uploads fail the run. Upload
scratch files through a writer to retain them.

## Connect to providers

Call `access_token()` for a fresh token when needed. Keep token values out of
logs and serialized data; provider refresh credentials stay with the host.

Local runs read tokens from `SLOPER_TOKEN_<CONNECTION>`. Uppercase the
connection name and replace hyphens with underscores. Use
`sloper-extension run --help` for resource and source fixtures.

## Use guest libraries

Use WASI-compatible crates. The host provides outbound TCP, DNS, clocks,
randomness, and a disposable `/tmp` directory. Access it through `std::fs` and
`std::env::current_dir()`. Other host paths and inbound listeners are
unavailable.

Dispatch provides a current-thread Tokio runtime. Use its tasks, sockets, and
timers. Thread-dependent APIs such as `spawn_blocking`, `tokio::fs`,
`reqwest::blocking`, and multithreaded Tokio are unsupported. Long synchronous
work blocks cooperative scheduling. Dispatch drains accepted writes and uploads,
then cancels remaining background tasks.

For HTTP, follow the [runtime fixture](../tests/runtime-component/src/guest.rs),
its [features](../tests/runtime-component/Cargo.toml), and the
[WASI flags](../.cargo/config.toml). Reqwest needs a guest-thread DNS resolver
and WASI-compatible TLS with default features disabled. Keep SDK calls outside
`wstd::runtime::block_on`.

## Build and test

```sh
sloper-extension build
sloper-extension check
sloper-extension run normalize --resource records=records.jsonl --out run
```

Build writes `dist/extension.wasm`, preserves an optional icon, and copies the
declared license and any supplied `ThirdPartyNotices.txt`. Use a new output
directory for each local run. Test pure transformations natively and
capabilities through the component host.

`build` and `check` use size-optimized release builds (`opt-level = "s"`), ThinLTO,
and one code-generation unit, with debug information disabled and Cargo symbol
stripping enabled. These settings override the project's release profile and
matching Cargo profile environment variables.

Next, [validate](validation.md) or [publish](publishing.md) the component. See
[licensing](licensing.md) for distribution files.
