#!/usr/bin/env nu

# Regenerate the manifest schema and checksum from the public spec crate.
def main [] {
  cd ($env.FILE_PWD | path dirname)
  let output = ^cargo run --locked --quiet --package sloper-extension-spec --bin generate-extension-schema | into binary
  let schema = 'crates/sloper-extension-spec/schema/extension-manifest.schema.json'
  $output | save --raw --force $schema
  $"($output | hash sha256)  schema/extension-manifest.schema.json\n" | save --raw --force $"($schema).sha256"
}
