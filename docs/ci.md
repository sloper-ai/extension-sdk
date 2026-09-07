# Extension verification, delivery, and promotion

The source repository owns its workflow runs, staging and production environments, components, and receipts. The SDK provides reusable workflows and publication tools. Artifacts remain in the repository that runs the workflow.

The SDK's own `verify.yml` checks formatting, static validation and execution, guest examples, and delivery helper tests. It checks out only the SDK. A component with a trapping start function verifies that static validation accepts its structure while execution admission rejects it. Collection verification also installs the caller's pinned tools and runs its repository checks. Dependency notices come from `cargo-license`, the same generator the SDK uses for its own release archives.

## Configure a collection

Use Python 3.11 or later and the checked-in Rust toolchain. Each collection has an `extensions.json` file:

```json
{
  "extensions": [
    { "path": "gmail", "package": "gmail" }
  ]
}
```

Each listed Cargo package declares its own stable semantic version as a string, such as `version = "0.1.0"`. Workspace-inherited versions and prereleases are rejected. An empty collection uses `{"extensions":[]}` and needs no Cargo workspace.

Acceptance tests must consume the final `dist/extension.wasm` produced by `sloper-extension build`. They must not rebuild it. The shared workflow builds every listed extension before running `cargo test --locked --workspace --all-targets`, then verifies that the component bytes have not changed. Add an extension's behavior tests to its owning package. A new collection entry or an explicit version increase since the last successful staging publication creates a candidate. Once a version has staged successfully, source-only changes at that version run verification without creating another candidate. Decreasing a version fails.

Candidate selection uses the last successful staging job in a successful push-to-main delivery run, inspecting that run's latest attempt. Successful runs containing only checks or skipped staging do not advance this baseline. With no successful staging baseline, all collection entries remain candidates, including initial versions verified before publication was enabled.

After reviewing an immutable SDK commit, record its full SHA in `extension-sdk.rev` and in the `uses` references of `.github/workflows/deliver.yml` and `.github/workflows/promote.yml`. Both callers must reference that commit's reusable workflows, and the separately checked-out tooling must use the same revision. Update Cargo SDK Git revisions and lockfiles together, then review and commit the changes in the collection. Organization collections use `private` visibility; Sloper's public collection uses `public`.

## Configure each environment

The first push to `main` can build, test, and record all candidates without publication setup. Leave the collection repository variable `EXTENSION_PUBLICATION_ENABLED` unset to skip staging without selecting the `staging` environment or exposing publishing-token secrets. Before enabling publication, configure separate `staging` and `production` environments in the collection repository.

| Setting | Staging | Production |
| --- | --- | --- |
| `SLOPER_API_URL` environment variable | Staging Sloper HTTPS API origin | Production Sloper HTTPS API origin |
| `SLOPER_STAGING_API_URL` environment variable | Unused | Same staging origin used by the delivery receipt |
| `SLOPER_API_TOKEN` environment secret | Staging scoped publishing token | Separate production scoped publishing token |

Issue API tokens with `extensions.publish-new`, `extensions.publish-update`, and `extensions.read` as needed. Limit their resource access to the intended namespace and packages. The namespace's creation policy and the issuing user's package permissions determine which visibility can be published. Use separate tokens for staging and production. The repository name is provenance; it grants no publishing authority. See [publishing](publishing.md) for the API token and visibility contract.

The reusable publication jobs select these caller-owned environments. Publishing credentials are exposed only to the publish step, after trusted SDK tooling has been compiled in a fresh job. Build scripts, guest execution, and acceptance tests receive no publishing token. No `secrets: inherit` is needed in the thin callers.

After configuring both environments and their scoped tokens, set the collection repository variable `EXTENSION_PUBLICATION_ENABLED` to `true`. The next push to `main`, or a rerun of its delivery workflow, can publish nonempty candidate sets to staging. The thin delivery caller continues to use push and pull-request triggers; it has no manual dispatch trigger. When publication is enabled, a missing staging token or API URL fails explicitly.

## Deliver and promote

A pull request verifies and records candidates without publication. A push to `main` publishes nonempty candidate sets to staging after tests pass only when `EXTENSION_PUBLICATION_ENABLED` is `true`. Each caller run retains the candidate artifact; a staging run also retains its receipt:

- `extension-candidates-RUN_ID-ATTEMPT`: exact components and icons, exact manifest text, required licenses, and `delivery.json`.
- `extension-staging-receipt-RUN_ID-ATTEMPT`: `staging.json`, bound to the complete artifact metadata digest and verified Sloper release digests.

Metadata identifies the source repository, source SHA, SDK SHA, run ID, build attempt, visibility, extension IDs/versions, source packages, and component/manifest/icon digests and sizes. Run attempts have separate artifact names. Re-running a failed staging job retains the successful build's original artifact and writes a new publication-attempt receipt; it does not rebuild components. Candidate and staging artifacts expire after 30 days; promote while they are retained.

Run the collection's `promote` workflow on `main`, supplying its successful staging delivery run ID. A run with skipped staging has no staging receipt and cannot be promoted; promotion fails for the missing receipt. Promotion checks the run's repository, workflow, event, branch, conclusion, revision, attempt, and available artifacts. It verifies every file digest and the complete staging receipt before publishing. The collection's current trusted SDK pin must match the candidate's SDK revision; promote outstanding candidates before changing that pin.

Promotion checks out only trusted SDK tooling, downloads the candidate bytes, and invokes `sloper-extension publish`. It never rebuilds the component, restamps a version, or executes source-repository scripts. The production Sloper API validates and signs the same bytes independently. The production receipt is retained in the promotion run for 90 days.

A failed partial publication leaves its successfully verified receipts. Retrying resubmits every candidate using the same immutable bytes so the Sloper API checks current authority and performs identical-byte idempotent replay. A conflicting version fails; local receipts never replace Sloper API authorization. Missing or expired artifacts, changed digests, empty candidate sets, mismatched audiences, and incomplete staging receipts fail before production publication. An empty collection succeeds during verification and creates no staging publication.

## Organization CI without GitHub Actions

The reusable workflows are complete GitHub Actions callers for organization repositories. Other CI systems can use the same standalone commands and credential separation:

```sh
# Build/test job, with no SLOPER_API_TOKEN in its environment.
sloper-extension build path/to/extension --json
cargo test --locked --workspace --all-targets
sloper-extension validate path/to/extension/dist/extension.wasm --json
# Retain dist/extension.wasm and optional dist/icon.png as immutable CI artifacts.

# Publication job: restore the tested dist directory, then provide only this
# process with the CI system's scoped secret and the intended Sloper API origin.
sloper-extension publish path/to/restored-artifact \
  --api-url "$SLOPER_API_URL" --visibility private --json
```

The publishing command reads `SLOPER_API_TOKEN`, validates the exact component bytes, and verifies the Sloper API's returned envelope and digests. Its JSON result contains `version` and `envelope`; it does not require Cargo metadata or extension source files.

For the complete artifact and promotion spec, use `scripts/ci/extensions.py plan`, `build`, `publish`, and `resolve` as the reusable workflows do. Run helper verification with:

```sh
python3 -m py_compile scripts/ci/*.py
python3 -m unittest discover -s scripts/ci -p 'test_*.py' -v
actionlint .github/workflows/*.yml
```
