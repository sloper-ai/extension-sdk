#!/usr/bin/env nu

# Use the source commit's clock, so retries produce the same package version.
def snapshot [] {
    let commit = (^git rev-parse --verify HEAD | str trim)
    let timestamp = (^git show -s --format=%ct HEAD | str trim | into datetime --format '%s'
        | date to-timezone UTC | format date '%Y%m%d%H%M%S')
    let version = $'0.0.0-($timestamp).g($commit | str substring 0..11)'
    {commit: $commit, version: $version, tag: $'v($version)'}
}

def outputs [values: record] {
    if ($env.GITHUB_OUTPUT? | is-not-empty) {
        $values | transpose key value | each {|entry| $"($entry.key)=($entry.value)\n" }
            | str join | save --raw --append $env.GITHUB_OUTPUT
    }
}

# Listing releases keeps a missing tag distinct from an authentication/network error.
def published [release: record] {
    let repository = $'repos/($env.GH_REPO)'
    let tags = (^gh api $'($repository)/git/matching-refs/tags/($release.tag)' | from json
        | where ref == $'refs/tags/($release.tag)')
    if ($tags | is-not-empty) {
        let commit = (^gh api $'($repository)/commits/($release.tag)' --jq .sha | str trim)
        if $commit != $release.commit {
            error make {msg: $'Tag ($release.tag) belongs to another source commit: ($commit)'}
        }
    }
    let existing = (^gh api --paginate --slurp $'($repository)/releases?per_page=100'
        | from json | flatten | where tag_name == $release.tag | get 0?)
    if $existing == null { return false }
    if ($tags | is-empty) and $existing.target_commitish != $release.commit {
        error make {msg: $'Draft ($release.tag) belongs to another source commit'}
    }
    not $existing.draft
}

def main [] { help main }

def "main prepare" [--dry-run] {
    if not $dry_run and $env.GITHUB_REF? != 'refs/heads/release' {
        error make {msg: 'Publication must run from the release branch; use a dry run for other refs'}
    }
    if (^git status --porcelain | str trim | is-not-empty) {
        error make {msg: 'Release preparation requires a clean checkout'}
    }
    let release = (snapshot)
    let completed = if $dry_run { false } else { published $release }
    outputs ($release | insert completed $completed)
    if $completed {
        print $'($release.tag) is already published'
        return
    }

    ^mise run //:check
    ^cargo workspaces version custom $release.version --force '*' --exact --yes --no-git-commit
    # Preserve the selected revision in source-installed CLIs despite CI's version edits.
    $release.commit | save --raw --force crates/sloper-extension-cli/release-revision
    for fixture in [{directory: guest, builder: build-fixture}, {directory: sdk-guest, builder: build-sdk-fixture}] {
        let manifest = $'crates/sloper-extension-host/tests/($fixture.directory)/Cargo.toml'
        ^cargo update --workspace --manifest-path $manifest
        ^cargo run --locked --manifest-path $manifest --bin $fixture.builder --target-dir target
    }
    ^mise run '//...:build'
    ^mise run '//...:test'
    ^mise run //:package

    # Every build and publisher receives the same stamped manifests, locks and fixtures.
    mkdir .cache/release
    $release | to json | save --raw --force .cache/release/release.json
    let files = (^git ls-files | lines | append 'crates/sloper-extension-cli/release-revision')
    $files | str join "\n" | save --raw --force .cache/release/source-files.txt
    ^tar -czf .cache/release/source.tar.gz -T .cache/release/source-files.txt
}

def "main restore" [] {
    let release = (open .cache/release/release.json)
    if $release != (snapshot) {
        error make {msg: 'Prepared source does not match this checkout'}
    }
    ^tar -xzf .cache/release/source.tar.gz
}

def "main licenses" [target: string] {
    ^cargo metadata --locked --format-version 1 --filter-platform $target | ignore
    ^cargo license --manifest-path crates/sloper-extension-cli/Cargo.toml --filter-platform $target --avoid-dev-deps --tsv --output .cache/release/ThirdPartyNotices.txt
}

def "main stage" [] {
    let release = (open .cache/release/release.json)
    let completed = (published $release)
    outputs {completed: $completed}
    if $completed { return }

    let drafts = (^gh api --paginate --slurp $'repos/($env.GH_REPO)/releases?per_page=100'
        | from json | flatten | where tag_name == $release.tag)
    if ($drafts | is-empty) {
        ^gh release create $release.tag --target $release.commit --title $release.tag --draft --generate-notes --notes $'Source commit: ($release.commit)'
    }
    let assets = (glob .cache/release/assets/*)
    ^gh release upload $release.tag ...$assets .cache/release/release.json --clobber
}

def "main finish" [] {
    let release = (open .cache/release/release.json)
    if not (published $release) {
        ^gh release edit $release.tag --draft=false --latest
    }
}
