import argparse
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import extensions as ci


class DeliveryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = ci.provenance(
            "acme/extensions", "a" * 40, "b" * 40, "321", "2", "private"
        )

    def artifact(self, count=1):
        self.artifact_root = self.root / "artifact"
        self.artifact_root.mkdir(exist_ok=True)
        entries = []
        licenses = []
        for index in range(count):
            identity = f"acme.extension-{index}"
            prefix = "extensions/" + ci.hashlib.sha256(identity.encode()).hexdigest()
            directory = self.artifact_root / prefix
            (directory / "dist").mkdir(parents=True)
            (directory / "dist/extension.wasm").write_bytes(
                b"component " + bytes([index])
            )
            (directory / "manifest.json").write_text(
                json.dumps({"name": identity, "version": "1.2.3"}), encoding="utf-8"
            )
            (directory / "dist/LICENSE").write_text(
                "Extension license\n", encoding="utf-8"
            )
            licenses.append(
                ci.file_record(self.artifact_root, prefix + "/dist/LICENSE")
            )
            entries.append(
                {
                    "id": identity,
                    "version": "1.2.3",
                    "package": f"extension-{index}",
                    "source_path": f"extension-{index}",
                    "component": ci.file_record(
                        self.artifact_root, prefix + "/dist/extension.wasm"
                    ),
                    "manifest": ci.file_record(
                        self.artifact_root, prefix + "/manifest.json"
                    ),
                    "icon": None,
                }
            )
        (self.artifact_root / "LICENSE").write_text(
            "Test-only collection license\n", encoding="utf-8"
        )
        self.metadata = {
            "schema": 1,
            "source": self.source,
            "extensions": entries,
            "licenses": [ci.file_record(self.artifact_root, "LICENSE"), *licenses],
        }
        ci.write_json(self.artifact_root / "delivery.json", self.metadata)
        return self.metadata

    def response(self, entry):
        envelope_bytes = json.dumps(
            {"subject": entry["id"], "component": entry["component"]["digest"]}
        )
        envelope_id = ci.digest(envelope_bytes.encode())
        return {
            "version": {
                "extension_id": entry["id"],
                "version": entry["version"],
                "state": "published",
                "release_envelope_id": envelope_id,
            },
            "envelope": {
                "id": envelope_id,
                "extension_id": entry["id"],
                "version": entry["version"],
                "envelope": envelope_bytes,
                "manifest": ci.within(
                    self.artifact_root, entry["manifest"]["path"]
                ).read_text(),
                "component": {
                    "sha256": entry["component"]["digest"],
                    "bytes": entry["component"]["bytes"],
                },
            },
        }

    def args(self, environment="staging"):
        return argparse.Namespace(
            repository=self.source["repository"],
            revision=self.source["revision"],
            sdk_revision=self.source["sdk_revision"],
            run_id=self.source["run_id"],
            attempt=self.source["attempt"],
            visibility="private",
            artifact=self.artifact_root,
            api_url=f"https://{environment}.example.test",
            environment=environment,
            tool=Path("/sdk/sloper-extension"),
            receipt=self.root / f"{environment}.json",
            staging_receipt=self.root / "staging.json",
            staging_api_url="https://staging.example.test",
        )

    def test_new_and_incremented_versions_publish_but_source_only_changes_do_not(self):
        old = [{"path": "gmail", "package": "gmail", "version": "1.0.0"}]
        self.assertEqual(ci.candidates(old, old), [])
        bump = [{"path": "gmail", "package": "gmail", "version": "1.1.0"}]
        self.assertEqual(ci.candidates(bump, old), bump)
        new = {"path": "calendar", "package": "calendar", "version": "0.1.0"}
        self.assertEqual(ci.candidates([*old, new], old), [new])

    def test_explicit_baseline_requires_an_immutable_revision(self):
        with self.assertRaisesRegex(ci.DeliveryError, "full Git commit SHA"):
            ci.plan(argparse.Namespace(base="main"))

    def test_checks_only_runs_do_not_consume_initial_publication_candidates(self):
        runs = {
            "workflow_runs": [
                {"id": 12, "run_attempt": 1, "head_sha": "a" * 40},
                {"id": 11, "run_attempt": 2, "head_sha": "b" * 40},
            ]
        }
        skipped = {
            "total_count": 2,
            "jobs": [
                {"name": "deliver / verify", "conclusion": "success"},
                {"name": "deliver / " + ci.STAGING_JOB_NAME, "conclusion": "skipped"},
            ],
        }
        with patch.object(ci, "github", side_effect=[runs, skipped]) as request:
            self.assertIsNone(ci.last_delivery("acme/extensions", "12"))
        self.assertIn("/runs/11/attempts/2/jobs", request.call_args.args[0])
        initial = [{"path": "mail", "package": "mail", "version": "0.1.0"}]
        self.assertEqual(ci.candidates(initial, []), initial)

    def test_baseline_uses_successful_staging_after_later_checks_only_runs(self):
        runs = {
            "workflow_runs": [
                {"id": 12, "run_attempt": 1, "head_sha": "a" * 40},
                {"id": 11, "run_attempt": 2, "head_sha": "b" * 40},
            ]
        }
        skipped = {
            "total_count": 1,
            "jobs": [{"name": ci.STAGING_JOB_NAME, "conclusion": "skipped"}],
        }
        staged = {
            "total_count": 1,
            "jobs": [
                {"name": "deliver / " + ci.STAGING_JOB_NAME, "conclusion": "success"}
            ],
        }
        with patch.object(ci, "github", side_effect=[runs, skipped, staged]):
            self.assertEqual(ci.last_delivery("acme/extensions", "13"), "b" * 40)

    def test_baseline_search_fails_instead_of_guessing_after_history_limit(self):
        runs = {
            "workflow_runs": [{"id": 12, "run_attempt": 1, "head_sha": "a" * 40}] * 100
        }
        with (
            patch.object(ci, "github", return_value=runs),
            self.assertRaisesRegex(ci.DeliveryError, "1,000-run search limit"),
        ):
            ci.last_delivery("acme/extensions", "12")

    def test_version_decreases_and_implicit_versions_fail(self):
        with self.assertRaises(ci.DeliveryError):
            ci.candidates(
                [{"path": "a", "package": "a", "version": "1.0.0"}],
                [{"path": "a", "package": "a", "version": "2.0.0"}],
            )
        for version in [{"workspace": True}, "01.0.0", "1.0", "1.0.0-beta"]:
            with self.subTest(version=version), self.assertRaises(ci.DeliveryError):
                ci.version_tuple(version)

    def test_collection_reads_explicit_package_identity_and_version(self):
        (self.root / "gmail").mkdir()
        (self.root / "gmail/Cargo.toml").write_text(
            '[package]\nname="gmail"\nversion="0.1.0"\n', encoding="utf-8"
        )
        ci.write_json(
            self.root / "extensions.json",
            {"extensions": [{"path": "gmail", "package": "gmail"}]},
        )
        self.assertEqual(
            ci.collection(self.root),
            [{"path": "gmail", "package": "gmail", "version": "0.1.0"}],
        )

    def test_artifact_rejects_tampering_and_wrong_repository_run_or_audience(self):
        self.artifact()
        self.assertEqual(
            ci.verify_artifact(self.artifact_root, self.source), self.metadata
        )
        for field, value in [
            ("repository", "acme/other-extensions"),
            ("visibility", "public"),
            ("run_id", "100"),
            ("attempt", "1"),
            ("sdk_revision", "c" * 40),
        ]:
            with self.subTest(field=field), self.assertRaises(ci.DeliveryError):
                ci.verify_artifact(self.artifact_root, {**self.source, field: value})
        (component,) = self.artifact_root.glob("extensions/*/dist/extension.wasm")
        component.write_bytes(b"replacement")
        with self.assertRaises(ci.DeliveryError):
            ci.verify_artifact(self.artifact_root, self.source)

    def test_paths_and_symlinks_cannot_escape_artifact(self):
        for path in ["../secret", "/secret", "a/../../secret", "a\\secret"]:
            with self.subTest(path=path), self.assertRaises(ci.DeliveryError):
                ci.within(self.root, path)
        (self.root / "link").symlink_to(self.root)
        with self.assertRaises(ci.DeliveryError):
            ci.within(self.root, "link/file")

    def test_missing_licenses_fail_artifact_verification(self):
        self.artifact()
        (self.artifact_root / "LICENSE").unlink()
        with self.assertRaises(FileNotFoundError):
            ci.verify_artifact(self.artifact_root, self.source)

    def test_empty_collection_succeeds_without_invoking_cargo_or_publishing(self):
        ci.write_json(
            self.root / "plan.json",
            {"schema": 1, "source": self.source, "extensions": [], "candidates": []},
        )
        (self.root / "LICENSE").write_text("Test-only license\n", encoding="utf-8")
        args = argparse.Namespace(
            source=self.root,
            plan=self.root / "plan.json",
            tool=Path("/no/tool"),
            output=self.root / "empty",
        )
        with patch.object(
            ci,
            "run",
            side_effect=AssertionError("Empty collections must not invoke Cargo"),
        ):
            ci.build(args)
        ci.verify_artifact(args.output, self.source, allow_empty=True)
        with self.assertRaises(ci.DeliveryError):
            ci.verify_artifact(args.output, self.source)

    def test_build_refuses_publication_credentials(self):
        ci.write_json(self.root / "plan.json", {"schema": 1})
        args = argparse.Namespace(plan=self.root / "plan.json")
        with (
            patch.dict(ci.os.environ, {"SLOPER_API_TOKEN": "test-token"}),
            self.assertRaises(ci.DeliveryError),
        ):
            ci.build(args)

    def test_acceptance_test_cannot_replace_distributable_bytes(self):
        source = self.root / "source"
        (source / "gmail/dist").mkdir(parents=True)
        component = source / "gmail/dist/extension.wasm"
        component.write_bytes(b"built component")
        entry = {"path": "gmail", "package": "gmail", "version": "1.0.0"}
        ci.write_json(
            self.root / "plan.json",
            {
                "schema": 1,
                "source": self.source,
                "extensions": [entry],
                "candidates": [entry],
            },
        )
        args = argparse.Namespace(
            source=source, plan=self.root / "plan.json", tool=Path("tool")
        )
        with (
            patch.object(ci, "tool_json", return_value={}),
            patch.object(
                ci,
                "run",
                side_effect=lambda *args, **kwargs: component.write_bytes(b"changed"),
            ),
            self.assertRaises(ci.DeliveryError),
        ):
            ci.build(args)

    def test_publish_checks_remote_identity_digests_and_release_state(self):
        entry = self.artifact()["extensions"][0]
        response = self.response(entry)
        receipt = ci.release_receipt(response, entry)
        self.assertEqual(receipt["component_digest"], entry["component"]["digest"])
        response["envelope"]["manifest"] += " "
        with self.assertRaises(ci.DeliveryError):
            ci.release_receipt(response, entry)
        response = self.response(entry)
        response["version"]["state"] = "withdrawn"
        with self.assertRaises(ci.DeliveryError):
            ci.release_receipt(response, entry)

    def test_package_preserves_manifest_bytes_icon_and_dependency_licenses(self):
        source = self.root / "source"
        (source / "gmail/dist").mkdir(parents=True)
        (source / "LICENSE").write_text("Collection license\n", encoding="utf-8")
        (source / "gmail/dist/extension.wasm").write_bytes(b"tested component")
        (source / "gmail/dist/icon.png").write_bytes(b"tested icon")
        (source / "gmail/dist/LICENSE").write_bytes(b"Extension license\r\n")
        (source / "gmail/dist/ThirdPartyNotices.txt").write_bytes(
            b"Dependency license\n"
        )
        entry = {"path": "gmail", "package": "gmail", "version": "1.0.0"}
        manifest = ' { "name": "acme.gmail", "version": "1.0.0" }\n'
        args = argparse.Namespace(
            source=source, tool=Path("sloper-extension"), output=self.root / "packaged"
        )

        value = {"source": self.source, "extensions": [entry], "candidates": [entry]}
        with (
            patch.object(
                ci, "tool_json", return_value={"valid": True, "manifest": manifest}
            ) as validate,
            patch.object(
                ci,
                "run",
                side_effect=AssertionError("Packaging must not generate licenses"),
            ),
        ):
            ci.package(args, value)
        validate.assert_called_once_with(
            args.tool, "validate", source / "gmail/dist/extension.wasm", "--json"
        )
        metadata = ci.verify_artifact(args.output, self.source)
        built = metadata["extensions"][0]
        self.assertEqual(
            ci.verify_file(args.output, built["manifest"]), manifest.encode()
        )
        self.assertEqual(
            ci.verify_file(args.output, built["component"]), b"tested component"
        )
        self.assertEqual(ci.verify_file(args.output, built["icon"]), b"tested icon")
        prefix = "extensions/" + ci.hashlib.sha256(b"acme.gmail").hexdigest()
        self.assertEqual(
            {
                license_record["path"]: ci.verify_file(args.output, license_record)
                for license_record in metadata["licenses"]
            },
            {
                "LICENSE": b"Collection license\n",
                prefix + "/dist/LICENSE": b"Extension license\r\n",
                prefix + "/dist/ThirdPartyNotices.txt": b"Dependency license\n",
            },
        )
        (args.output / prefix / "dist/ThirdPartyNotices.txt").write_bytes(b"changed")
        with self.assertRaisesRegex(ci.DeliveryError, "bytes changed"):
            ci.verify_artifact(args.output, self.source)

    def test_candidate_must_include_its_built_license(self):
        source = self.root / "source"
        dist = source / "mail/dist"
        dist.mkdir(parents=True)
        (source / "LICENSE").write_text("Collection license\n")
        (dist / "extension.wasm").write_bytes(b"tested component")
        entry = {"path": "mail", "package": "mail", "version": "1.0.0"}
        value = {"source": self.source, "extensions": [entry], "candidates": [entry]}
        args = argparse.Namespace(
            source=source, output=self.root / "missing", tool=Path("tool")
        )
        with patch.object(
            ci,
            "tool_json",
            return_value={
                "valid": True,
                "manifest": '{"name":"acme.mail","version":"1.0.0"}',
            },
        ):
            with self.assertRaisesRegex(ci.DeliveryError, "dist/LICENSE"):
                ci.package(args, value)
            (dist / "LICENSE").write_bytes(b"Extension license")
            args.output = self.root / "licensed"
            ci.package(args, value)
        metadata = ci.verify_artifact(args.output, self.source)
        self.assertEqual(len(metadata["licenses"]), 2)
        metadata["licenses"] = metadata["licenses"][:1]
        ci.write_json(args.output / "delivery.json", metadata)
        with self.assertRaisesRegex(ci.DeliveryError, "required LICENSE"):
            ci.verify_artifact(args.output, self.source)

    def test_partial_retry_rechecks_every_release_with_original_bytes(self):
        entries = self.artifact(2)["extensions"]
        args = self.args()
        before = [
            (
                entry["component"]["digest"],
                ci.within(self.artifact_root, entry["component"]["path"]).read_bytes(),
            )
            for entry in entries
        ]
        with (
            patch.object(
                ci,
                "tool_json",
                side_effect=[
                    self.response(entries[0]),
                    ci.DeliveryError("interrupted"),
                ],
            ),
            self.assertRaises(ci.DeliveryError),
        ):
            ci.publish(args)
        self.assertEqual(len(ci.read_json(args.receipt)["releases"]), 1)
        with patch.object(
            ci, "tool_json", side_effect=[self.response(entry) for entry in entries]
        ) as publish:
            ci.publish(args)
        self.assertEqual(publish.call_count, 2)
        self.assertEqual(len(ci.read_json(args.receipt)["releases"]), 2)
        for entry, (expected_digest, expected_bytes) in zip(entries, before):
            self.assertEqual(
                ci.within(self.artifact_root, entry["component"]["path"]).read_bytes(),
                expected_bytes,
            )
            self.assertEqual(entry["component"]["digest"], expected_digest)

    def test_production_requires_complete_matching_staging_receipt(self):
        entry = self.artifact()["extensions"][0]
        production = self.args("production")
        with self.assertRaises(FileNotFoundError):
            ci.publish(production)
        with patch.object(ci, "tool_json", return_value=self.response(entry)):
            ci.publish(self.args())
        staging = ci.read_json(production.staging_receipt)
        bad = {**staging, "api_url": "https://unrelated.example.test"}
        ci.write_json(production.staging_receipt, bad)
        with self.assertRaises(ci.DeliveryError):
            ci.publish(production)
        ci.write_json(production.staging_receipt, staging)
        with patch.object(
            ci, "tool_json", return_value=self.response(entry)
        ) as publishing:
            ci.publish(production)
        self.assertEqual(publishing.call_count, 1)

    def test_resolver_rejects_wrong_workflow_and_expired_artifacts(self):
        args = argparse.Namespace(
            repository=self.source["repository"],
            run_id="321",
            sdk_revision="b" * 40,
            visibility="private",
            output=self.root / "resolved.json",
        )
        selected = {
            "repository": {"full_name": args.repository},
            "id": 321,
            "head_branch": "main",
            "event": "push",
            "conclusion": "success",
            "path": ".github/workflows/deliver.yml",
            "head_sha": "a" * 40,
            "run_attempt": 2,
        }
        artifacts = {
            "artifacts": [
                {"name": name + "-321-2", "expired": False}
                for name in [ci.ARTIFACT_NAME, ci.RECEIPT_NAME]
            ]
        }
        with patch.object(ci, "github", side_effect=[selected, artifacts]):
            ci.resolve(args)
        self.assertEqual(ci.read_json(args.output), self.source)
        # Re-running only a failed staging job keeps the successful original
        # build artifact and emits a new receipt without rebuilding components.
        retried = {
            "artifacts": [
                artifacts["artifacts"][0],
                {"name": ci.RECEIPT_NAME + "-321-3", "expired": False},
            ]
        }
        with patch.object(
            ci, "github", side_effect=[{**selected, "run_attempt": 3}, retried]
        ):
            ci.resolve(args)
        self.assertEqual(ci.read_json(args.output), self.source)
        with (
            patch.object(
                ci,
                "github",
                return_value={**selected, "path": ".github/workflows/untrusted.yml"},
            ),
            self.assertRaises(ci.DeliveryError),
        ):
            ci.resolve(args)
        artifacts["artifacts"][0]["expired"] = True
        with (
            patch.object(ci, "github", side_effect=[selected, artifacts]),
            self.assertRaises(ci.DeliveryError),
        ):
            ci.resolve(args)


if __name__ == "__main__":
    unittest.main()
