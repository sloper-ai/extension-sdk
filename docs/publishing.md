# Publish an extension

Build and test your extension, then publish its saved distributable:

```sh
sloper-extension publish ./acme.records --visibility private --json
```

Set `SLOPER_API_TOKEN` through your shell's secret manager or CI secret
environment. The command uploads `dist/extension.wasm` and an optional
`dist/icon.png`. The Sloper API validates, signs, and stores those files as an
immutable version.

Use `--api-url` to select a staging origin. The default is
`https://console.sloper.ai`; the client calls its `/api/v1` endpoints.

## Choose an audience

Use `private` for access controlled by package permissions, or `public` for
packages readable by authenticated Sloper users. The namespace's creation
policy determines who may create packages and which visibility they may choose.
The extension name must start with its publisher's namespace.

First publication fixes the extension's visibility. Later versions inherit it.
An API token needs `extensions.publish-new` for creation,
`extensions.publish-update` for subsequent versions, and `extensions.read` for
release receipt retrieval. Its optional resource restrictions and the issuing
user's current package permissions also apply.

Private organization packages can be installed only for their owning
organization. Private user packages can be installed for a personal owner that
has the required package read permission.

## Publish a new version

Bump the extension's version, rebuild, test, and publish. Extension versions are
independent of SDK and application versions.

Publishing the same component bytes at the same version returns the existing
release. Different bytes at that version cause a conflict. Retries still require
a valid token and current publishing authority.

Extension repositories own their release automation. Retain the tested `dist`
files between build and publication, and supply publishing credentials only to
the publish step.

Source licenses and release visibility are separate. See
[licensing](licensing.md).

## Call the publication API

Use `sloper-extension api` to call individual publication operations. These commands
accept the API's explicit parameters and print its response:

```sh
sloper-extension api extensions get-extension-version \
  --extension acme.records --version 1.0.0 --json
sloper-extension api extensions create-extension-version --help
```

Supply `SLOPER_API_TOKEN` as for `publish`. Use `--api-url` or `SLOPER_API_URL` to
select the endpoint. The `create-extension-version` command accepts `--component`
and `--icon` file paths and requires an explicit `--idempotency-key`.

The higher-level `publish` command validates the saved distributable, derives its
retry key, and checks the release receipt against the uploaded bytes.
