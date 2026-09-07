#!/usr/bin/env python3
"""Build, record, stage, and promote the same extension bytes in caller-owned CI."""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
import urllib.parse
import urllib.request
from pathlib import Path, PurePosixPath

SCHEMA = 1
SHA = re.compile(r"[a-f0-9]{40}\Z")
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
ARTIFACT_NAME = "extension-candidates"
RECEIPT_NAME = "extension-staging-receipt"
STAGING_JOB_NAME = "Publish candidates to staging"


class DeliveryError(Exception):
    """A delivery input or operation did not satisfy the release specification."""


def require(condition, message):
    if not condition:
        raise DeliveryError(message)


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def read_json(path):
    with path.open(encoding="utf-8") as stream:
        return json.load(stream)


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    temporary.replace(path)


def within(root, relative):
    require(
        isinstance(relative, str) and bool(relative), "An artifact path is missing."
    )
    path = PurePosixPath(relative)
    require(
        not path.is_absolute() and ".." not in path.parts and "\\" not in relative,
        f"Unsafe relative path: {relative}",
    )
    target = root.joinpath(*path.parts)
    require(
        target.resolve().is_relative_to(root.resolve()),
        f"Path escapes its root: {relative}",
    )
    for index in range(1, len(path.parts) + 1):
        require(
            not root.joinpath(*path.parts[:index]).is_symlink(),
            f"Symlinks are not allowed in delivery inputs: {relative}",
        )
    return target


def run(command, *, cwd=None, capture=False):
    result = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    )
    require(
        result.returncode == 0,
        f"Command failed with exit {result.returncode}: {command[0]}",
    )
    return result.stdout if capture else None


def tool_json(binary, *arguments):
    return json.loads(run([str(binary), *map(str, arguments)], capture=True))


def version_tuple(version):
    require(
        isinstance(version, str) and SEMVER.fullmatch(version),
        f"Extensions must declare an explicit stable semantic version: {version}",
    )
    return tuple(map(int, version.split(".")))


def collection(source, revision=None):
    def read(relative):
        if revision:
            return run(
                ["git", "show", f"{revision}:{relative}"], cwd=source, capture=True
            )
        return within(source, relative).read_text(encoding="utf-8")

    configuration = json.loads(read("extensions.json"))
    require(
        set(configuration) == {"extensions"}
        and isinstance(configuration["extensions"], list),
        "extensions.json must contain an extensions array.",
    )
    result = []
    seen = set()
    for extension in configuration["extensions"]:
        require(
            isinstance(extension, dict) and set(extension) == {"path", "package"},
            "Each collection entry requires exactly path and package.",
        )
        path, package = extension["path"], extension["package"]
        within(source, path)
        require(
            isinstance(package, str) and re.fullmatch(r"[A-Za-z0-9_-]+", package),
            "The Cargo package name is invalid.",
        )
        require(path not in seen, f"Duplicate extension path: {path}")
        seen.add(path)
        manifest = tomllib.loads(read(f"{path}/Cargo.toml"))["package"]
        require(manifest["name"] == package, f"Package mismatch for {path}.")
        version = manifest.get("version")
        version_tuple(version)
        result.append({"path": path, "package": package, "version": version})
    return result


def candidates(current, previous):
    old = {entry["path"]: entry for entry in previous}
    selected = []
    for entry in current:
        previous_entry = old.get(entry["path"])
        if previous_entry is None:
            selected.append(entry)
            continue
        require(
            entry["package"] == previous_entry["package"],
            f"Changing an extension package at {entry['path']} needs a new collection path.",
        )
        current_version = version_tuple(entry["version"])
        previous_version = version_tuple(previous_entry["version"])
        require(
            current_version >= previous_version,
            f"Version moved backwards at {entry['path']}.",
        )
        if current_version > previous_version:
            selected.append(entry)
    return selected


def github(path):
    token = os.environ.get("GITHUB_TOKEN")
    require(bool(token), "GITHUB_TOKEN is required for GitHub run inspection.")
    request = urllib.request.Request(
        "https://api.github.com/" + path,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def last_delivery(repository, run_id):
    require(REPOSITORY.fullmatch(repository), "Invalid source repository.")
    # Verification-only runs must not consume unpublished versions. Inspect the
    # successful staging job, including the selected run's latest attempt.
    for page in range(1, 11):
        query = urllib.parse.urlencode(
            {
                "branch": "main",
                "event": "push",
                "status": "success",
                "per_page": 100,
                "page": page,
            }
        )
        runs = github(f"repos/{repository}/actions/workflows/deliver.yml/runs?{query}")[
            "workflow_runs"
        ]
        for candidate in runs:
            if str(candidate["id"]) == str(run_id):
                continue
            jobs = github(
                f"repos/{repository}/actions/runs/{candidate['id']}/attempts/{candidate['run_attempt']}/jobs?per_page=100"
            )
            require(
                jobs["total_count"] <= 100,
                "Delivery run contains more jobs than the supported collection workflow.",
            )
            if any(
                job["name"].rsplit(" / ", 1)[-1] == STAGING_JOB_NAME
                and job["conclusion"] == "success"
                for job in jobs["jobs"]
            ):
                require(
                    SHA.fullmatch(candidate["head_sha"]),
                    "GitHub returned an invalid source revision.",
                )
                return candidate["head_sha"]
        if len(runs) < 100:
            return None
    raise DeliveryError(
        "No staged baseline found within GitHub's 1,000-run search limit; supply an explicit baseline."
    )


def provenance(repository, revision, sdk_revision, run_id, attempt, visibility):
    require(REPOSITORY.fullmatch(repository), "Invalid source repository.")
    require(
        SHA.fullmatch(revision) and SHA.fullmatch(sdk_revision),
        "Source and SDK revisions must be immutable Git SHAs.",
    )
    require(
        str(run_id).isdigit()
        and int(run_id) > 0
        and str(attempt).isdigit()
        and int(attempt) > 0,
        "Delivery run ID and attempt must be positive integers.",
    )
    require(visibility in {"public", "private"}, "Invalid publication visibility.")
    return {
        "repository": repository,
        "revision": revision,
        "sdk_revision": sdk_revision,
        "run_id": str(run_id),
        "attempt": str(attempt),
        "visibility": visibility,
    }


def plan(args):
    require(
        args.base is None or SHA.fullmatch(args.base),
        "An explicit baseline must be a full Git commit SHA.",
    )
    current = collection(args.source)
    baseline = args.base or last_delivery(args.repository, args.run_id)
    previous = collection(args.source, baseline) if baseline else []
    actual = run(["git", "rev-parse", "HEAD"], cwd=args.source, capture=True).strip()
    require(
        actual == args.revision,
        "Checked-out caller revision does not match the delivery.",
    )
    value = {
        "schema": SCHEMA,
        "source": provenance(
            args.repository,
            args.revision,
            args.sdk_revision,
            args.run_id,
            args.attempt,
            args.visibility,
        ),
        "baseline": baseline,
        "extensions": current,
        "candidates": candidates(current, previous),
    }
    write_json(args.output, value)
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            stream.write(f"count={len(value['candidates'])}\n")
    print(
        f"Verified {len(current)} collection entries; {len(value['candidates'])} publication candidates."
    )


def file_record(root, relative):
    data = within(root, relative).read_bytes()
    return {"path": relative, "digest": digest(data), "bytes": len(data)}


def verify_file(root, record):
    require(
        isinstance(record, dict) and set(record) == {"path", "digest", "bytes"},
        "Invalid artifact file descriptor.",
    )
    data = within(root, record["path"]).read_bytes()
    require(
        digest(data) == record["digest"] and len(data) == record["bytes"],
        f"Artifact bytes changed: {record['path']}",
    )
    return data


def distributable_digests(directory):
    icon = directory / "dist/icon.png"
    return (
        digest((directory / "dist/extension.wasm").read_bytes()),
        digest(icon.read_bytes()) if icon.exists() else None,
    )


def build(args):
    value = read_json(args.plan)
    require(value["schema"] == SCHEMA, "Unsupported delivery plan schema.")
    require(
        not os.environ.get("SLOPER_API_TOKEN"),
        "Build and test must not receive a publishing token.",
    )
    before = {}
    for entry in value["extensions"]:
        directory = within(args.source, entry["path"])
        tool_json(args.tool, "build", directory, "--json")
        before[entry["path"]] = distributable_digests(directory)
    if value["extensions"]:
        run(
            ["cargo", "test", "--locked", "--workspace", "--all-targets"],
            cwd=args.source,
        )
    for entry in value["extensions"]:
        directory = within(args.source, entry["path"])
        require(
            before[entry["path"]] == distributable_digests(directory),
            f"Acceptance tests changed final distributable bytes at {entry['path']}.",
        )
    package(args, value)


def package(args, value):
    require(
        not args.output.exists(),
        "Artifact output already exists; use a fresh directory.",
    )
    args.output.mkdir(parents=True)
    license_file = within(args.source, "LICENSE")
    require(license_file.is_file(), "The collection must supply its LICENSE.")
    shutil.copyfile(license_file, args.output / "LICENSE")
    licenses = [file_record(args.output, "LICENSE")]
    entries = []
    identities = set()
    for entry in value["candidates"]:
        directory = within(args.source, entry["path"])
        component = directory / "dist/extension.wasm"
        checked = tool_json(args.tool, "validate", component, "--json")
        require(
            checked["valid"] is True and isinstance(checked["manifest"], str),
            "Static validator rejected a candidate.",
        )
        manifest = json.loads(checked["manifest"])
        identity = manifest["name"]
        require(
            isinstance(identity, str) and identity not in identities,
            "Duplicate or invalid extension identity.",
        )
        identities.add(identity)
        require(
            manifest["version"] == entry["version"],
            "Built component version differs from Cargo version.",
        )
        prefix = "extensions/" + hashlib.sha256(identity.encode()).hexdigest()
        destination = args.output / prefix
        (destination / "dist").mkdir(parents=True)
        shutil.copyfile(component, destination / "dist/extension.wasm")
        (destination / "manifest.json").write_bytes(checked["manifest"].encode("utf-8"))
        record = {
            "id": identity,
            "version": entry["version"],
            "package": entry["package"],
            "source_path": entry["path"],
            "component": file_record(args.output, prefix + "/dist/extension.wasm"),
            "manifest": file_record(args.output, prefix + "/manifest.json"),
            "icon": None,
        }
        if (directory / "dist/icon.png").exists():
            shutil.copyfile(directory / "dist/icon.png", destination / "dist/icon.png")
            record["icon"] = file_record(args.output, prefix + "/dist/icon.png")
        for name in ("LICENSE", "ThirdPartyNotices.txt"):
            source = within(args.source, entry["path"] + "/dist/" + name)
            require(
                name != "LICENSE" or source.is_file(),
                "The built extension must supply dist/LICENSE.",
            )
            if source.is_file():
                relative = prefix + "/dist/" + name
                shutil.copyfile(source, within(args.output, relative))
                licenses.append(file_record(args.output, relative))
        entries.append(record)
    write_json(
        args.output / "delivery.json",
        {
            "schema": SCHEMA,
            "source": value["source"],
            "extensions": entries,
            "licenses": licenses,
        },
    )
    print(
        f"Recorded {len(entries)} tested extensions and {len(licenses)} license files."
    )


def verify_artifact(root, expected, allow_empty=False):
    metadata = read_json(root / "delivery.json")
    require(
        metadata.get("schema") == SCHEMA and metadata.get("source") == expected,
        "Artifact provenance does not match the selected repository, revision, SDK, run, attempt, and audience.",
    )
    require(
        isinstance(metadata.get("extensions"), list),
        "Artifact extension list is invalid.",
    )
    require(
        allow_empty or bool(metadata["extensions"]),
        "This delivery contains no extension releases.",
    )
    ids = set()
    required_licenses = {"LICENSE"}
    allowed_licenses = {"LICENSE"}
    for entry in metadata["extensions"]:
        require(entry["id"] not in ids, "Duplicate extension identity in artifact.")
        ids.add(entry["id"])
        version_tuple(entry["version"])
        verify_file(root, entry["component"])
        manifest = json.loads(verify_file(root, entry["manifest"]))
        require(
            manifest["name"] == entry["id"] and manifest["version"] == entry["version"],
            "Manifest identity does not match artifact.",
        )
        prefix = "extensions/" + hashlib.sha256(entry["id"].encode()).hexdigest()
        required_licenses.add(prefix + "/dist/LICENSE")
        allowed_licenses.update(
            {prefix + "/dist/LICENSE", prefix + "/dist/ThirdPartyNotices.txt"}
        )
        require(
            entry["component"]["path"] == prefix + "/dist/extension.wasm"
            and entry["manifest"]["path"] == prefix + "/manifest.json",
            "Unexpected extension artifact layout.",
        )
        if entry["icon"] is not None:
            require(
                entry["icon"]["path"] == prefix + "/dist/icon.png",
                "Unexpected icon artifact layout.",
            )
            verify_file(root, entry["icon"])
    require(
        isinstance(metadata.get("licenses"), list) and bool(metadata["licenses"]),
        "Artifact licenses are missing.",
    )
    seen_licenses = set()
    for license_record in metadata["licenses"]:
        require(
            license_record["path"] in allowed_licenses
            and license_record["path"] not in seen_licenses,
            "Unexpected or duplicate license artifact path.",
        )
        seen_licenses.add(license_record["path"])
        verify_file(root, license_record)
    require(
        required_licenses <= seen_licenses, "Artifact is missing a required LICENSE."
    )
    return metadata


def release_receipt(result, entry):
    version, envelope = result["version"], result["envelope"]
    require(
        version["extension_id"] == entry["id"]
        and version["version"] == entry["version"]
        and version["state"] == "published",
        "Console returned a different extension version or unavailable release.",
    )
    require(
        envelope["id"] == version["release_envelope_id"]
        and envelope["extension_id"] == entry["id"]
        and envelope["version"] == entry["version"],
        "Console returned a different release envelope identity.",
    )
    require(
        digest(envelope["envelope"].encode("utf-8")) == envelope["id"],
        "Console envelope digest does not match its bytes.",
    )
    require(
        digest(envelope["manifest"].encode("utf-8")) == entry["manifest"]["digest"]
        and envelope["component"]["sha256"] == entry["component"]["digest"]
        and envelope["component"]["bytes"] == entry["component"]["bytes"],
        "Console release does not match the tested bytes.",
    )
    return {
        "id": entry["id"],
        "version": entry["version"],
        "release_envelope_id": envelope["id"],
        "component_digest": entry["component"]["digest"],
        "manifest_digest": entry["manifest"]["digest"],
        "icon_digest": entry["icon"]["digest"] if entry["icon"] else None,
    }


def receipt_matches(receipt, metadata, artifact_digest, environment, api_url):
    require(
        receipt.get("schema") == SCHEMA
        and receipt.get("source") == metadata["source"]
        and receipt.get("artifact_digest") == artifact_digest
        and receipt.get("environment") == environment
        and receipt.get("api_url") == api_url,
        "Publication receipt does not match this artifact and destination.",
    )
    require(isinstance(receipt.get("releases"), list), "Receipt releases are missing.")
    entries = {entry["id"]: entry for entry in metadata["extensions"]}
    seen = set()
    for release in receipt["releases"]:
        entry = entries.get(release["id"])
        if entry is None or release["id"] in seen:
            raise DeliveryError("Receipt contains an unrelated or repeated extension.")
        seen.add(release["id"])
        require(
            release["version"] == entry["version"]
            and release["component_digest"] == entry["component"]["digest"]
            and release["manifest_digest"] == entry["manifest"]["digest"]
            and release["icon_digest"]
            == (entry["icon"]["digest"] if entry["icon"] else None)
            and re.fullmatch(r"sha256:[a-f0-9]{64}", release["release_envelope_id"]),
            "Receipt release bytes or identity do not match.",
        )
    return seen


def publish(args):
    expected = provenance(
        args.repository,
        args.revision,
        args.sdk_revision,
        args.run_id,
        args.attempt,
        args.visibility,
    )
    metadata = verify_artifact(args.artifact, expected)
    artifact_digest = digest((args.artifact / "delivery.json").read_bytes())
    require(
        args.api_url.startswith("https://"),
        "CI publication requires an HTTPS Console API URL.",
    )
    if args.environment == "production":
        require(
            args.staging_receipt is not None and args.staging_api_url,
            "Production requires a staging receipt and expected staging API URL.",
        )
        staged = receipt_matches(
            read_json(args.staging_receipt),
            metadata,
            artifact_digest,
            "staging",
            args.staging_api_url,
        )
        require(
            staged == {entry["id"] for entry in metadata["extensions"]},
            "Staging did not publish every candidate.",
        )
    receipt = {
        "schema": SCHEMA,
        "source": metadata["source"],
        "artifact_digest": artifact_digest,
        "environment": args.environment,
        "api_url": args.api_url,
        "releases": [],
    }
    if args.receipt.exists():
        receipt = read_json(args.receipt)
        receipt_matches(
            receipt, metadata, artifact_digest, args.environment, args.api_url
        )
    # Replay all entries: Console rechecks current authority and verifies immutable bytes.
    # A local receipt must never stand in for server authorization on retry.
    releases = []
    receipt["releases"] = releases
    write_json(args.receipt, receipt)
    for entry in metadata["extensions"]:
        directory = within(args.artifact, entry["component"]["path"]).parent.parent
        result = tool_json(
            args.tool,
            "publish",
            directory,
            "--api-url",
            args.api_url,
            "--visibility",
            args.visibility,
            "--json",
        )
        releases.append(release_receipt(result, entry))
        write_json(args.receipt, receipt)
        print(f"Published {entry['id']} {entry['version']} to {args.environment}.")


def resolve(args):
    require(
        REPOSITORY.fullmatch(args.repository) and str(args.run_id).isdigit(),
        "Invalid promotion run reference.",
    )
    selected = github(f"repos/{args.repository}/actions/runs/{args.run_id}")
    require(
        selected["repository"]["full_name"] == args.repository
        and str(selected["id"]) == str(args.run_id),
        "Run belongs to another repository.",
    )
    require(
        selected["head_branch"] == "main"
        and selected["event"] == "push"
        and selected["conclusion"] == "success"
        and selected["path"] == ".github/workflows/deliver.yml",
        "Promotion requires a successful main delivery run.",
    )
    artifacts = github(
        f"repos/{args.repository}/actions/runs/{args.run_id}/artifacts?per_page=100"
    )["artifacts"]
    candidate_pattern = re.compile(rf"{ARTIFACT_NAME}-{args.run_id}-([1-9][0-9]*)\Z")
    attempts = [
        int(match[1])
        for artifact in artifacts
        if (match := candidate_pattern.fullmatch(artifact["name"]))
    ]
    require(bool(attempts), "The selected run has no candidate artifact.")
    candidate_attempt = max(attempts)
    require(
        candidate_attempt <= selected["run_attempt"],
        "Candidate artifact claims a future run attempt.",
    )
    expected = provenance(
        args.repository,
        selected["head_sha"],
        args.sdk_revision,
        args.run_id,
        candidate_attempt,
        args.visibility,
    )
    for name in [
        f"{ARTIFACT_NAME}-{args.run_id}-{candidate_attempt}",
        f"{RECEIPT_NAME}-{args.run_id}-{selected['run_attempt']}",
    ]:
        matches = [
            artifact
            for artifact in artifacts
            if artifact["name"] == name and not artifact["expired"]
        ]
        require(
            len(matches) == 1,
            f"The selected run has no unique, unexpired {name} artifact.",
        )
    write_json(args.output, expected)
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            stream.write(
                f"revision={expected['revision']}\nattempt={expected['attempt']}\nreceipt_attempt={selected['run_attempt']}\n"
            )


def add_source_arguments(parser):
    for name in ["repository", "revision", "sdk-revision", "run-id", "attempt"]:
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--visibility", required=True, choices=["public", "private"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    planning = commands.add_parser("plan")
    add_source_arguments(planning)
    planning.add_argument("--source", type=Path, required=True)
    planning.add_argument("--base")
    planning.add_argument("--output", type=Path, required=True)
    planning.set_defaults(handler=plan)
    building = commands.add_parser("build")
    for argument in ["source", "plan", "tool", "output"]:
        building.add_argument("--" + argument, type=Path, required=True)
    building.set_defaults(handler=build)
    publishing = commands.add_parser("publish")
    add_source_arguments(publishing)
    for argument in ["artifact", "tool", "receipt"]:
        publishing.add_argument("--" + argument, type=Path, required=True)
    publishing.add_argument(
        "--environment", required=True, choices=["staging", "production"]
    )
    publishing.add_argument("--api-url", required=True)
    publishing.add_argument("--staging-api-url")
    publishing.add_argument("--staging-receipt", type=Path)
    publishing.set_defaults(handler=publish)
    resolving = commands.add_parser("resolve")
    for argument in ["repository", "run-id", "sdk-revision"]:
        resolving.add_argument("--" + argument, required=True)
    resolving.add_argument("--visibility", required=True, choices=["public", "private"])
    resolving.add_argument("--output", type=Path, required=True)
    resolving.set_defaults(handler=resolve)
    args = parser.parse_args()
    try:
        args.handler(args)
    except (DeliveryError, OSError, ValueError, KeyError, TypeError) as error:
        print(f"Extension delivery failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
