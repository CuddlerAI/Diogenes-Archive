#!/usr/bin/env python3
"""Validate issue #609's machine-readable classification for every installed game."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
import sys
from datetime import date
from pathlib import Path, PureWindowsPath
from typing import Any
from urllib.parse import urlparse


DATAMINER_ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = DATAMINER_ROOT.parent
DEFAULT_MANIFEST = DATAMINER_ROOT / "config" / "game-depth-final-classifications.json"

EXPECTED_APP_IDS = {
    "8870",
    "227300",
    "261550",
    "377160",
    "752590",
    "976730",
    "1466860",
    "1771300",
    "1091500",
    "1240440",
    "2622380",
    "1145350",
    "1150690",
    "206440",
    "3561220",
    "4532590",
    "4570720",
}
ALLOWED_CLASSIFICATIONS = {"adapted", "blocked", "not_adaptable"}
ALLOWED_CODES = {
    "G1_EMBEDDED_ANTI_TAMPER_NO_DISABLED_MODE",
    "G1_KERNEL_ANTI_CHEAT",
    "G3_PROTECTED_ONLINE_PRODUCT",
    "G8_NO_CERTIFIABLE_CAMERA_PLAYER_CHANNEL",
    "G8_FIXED_ISOMETRIC_NO_ACTION_CAMERA",
    "G8_NO_3D_ACTION_CAMERA_DOMAIN",
    "G8_FIXED_2D_BULLET_HELL_CAMERA",
    "OUT_OF_SCOPE_UTILITY_NOT_GAME",
    "B_PROJECTION_CAMERA_UNRESOLVED",
    "B_PLAYER_POSE_EVIDENCE_MISSING",
    "B_60S_ZERO_REALIGN_EVIDENCE_MISSING",
    "B_METRIC_SCALE_SECOND_CHAIN_MISSING",
    "B_AUDIO_CATALOG_MISSING",
    "B_SCALE_LIVE_ANCHOR_MISSING",
}
ALLOWED_EVIDENCE_KINDS = {
    "official_store_disclosure",
    "official_store_taxonomy",
    "repository_record",
    "repository_survey",
}
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


def is_nonempty_string(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def valid_https_url(value: Any) -> bool:
    if not is_nonempty_string(value):
        return False
    parsed = urlparse(value)
    return parsed.scheme == "https" and bool(parsed.netloc)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def windows_relative_path(root: Path, value: Any) -> Path:
    if not is_nonempty_string(value):
        raise ValueError("relative path is empty")
    windows_path = PureWindowsPath(value)
    if windows_path.is_absolute() or ".." in windows_path.parts:
        raise ValueError(f"path is not a safe relative Windows path: {value}")
    candidate = root.joinpath(*windows_path.parts).resolve()
    candidate.relative_to(root.resolve())
    return candidate


def validate_installed(manifest: dict[str, Any], steamapps_root: Path) -> list[str]:
    errors: list[str] = []
    steamapps_root = steamapps_root.resolve()
    common_root = (steamapps_root / "common").resolve()
    if not steamapps_root.is_dir():
        return [f"steamapps root does not exist: {steamapps_root}"]
    if not common_root.is_dir():
        return [f"Steam common directory does not exist: {common_root}"]

    for record in manifest["records"]:
        app_id = record["app_id"]
        prefix = f"AppID {app_id}"
        try:
            executable = windows_relative_path(
                common_root, record["executable"]["relative_install_path"]
            )
        except ValueError as exc:
            errors.append(f"{prefix} executable path invalid: {exc}")
        else:
            if not executable.is_file():
                errors.append(f"{prefix} executable missing: {executable}")
            elif sha256_file(executable) != record["executable"]["sha256"]:
                errors.append(f"{prefix} executable SHA-256 drift: {executable}")

        appmanifest = steamapps_root / f"appmanifest_{app_id}.acf"
        if not appmanifest.is_file():
            errors.append(f"{prefix} Steam manifest missing: {appmanifest}")
        elif sha256_file(appmanifest) != record["appmanifest_sha256"]:
            errors.append(f"{prefix} Steam manifest SHA-256 drift: {appmanifest}")

        for security_file in record["installed_security_files"]:
            try:
                candidate = windows_relative_path(
                    common_root, security_file["relative_install_path"]
                )
            except ValueError as exc:
                errors.append(f"{prefix} security-file path invalid: {exc}")
                continue
            if not candidate.is_file():
                errors.append(f"{prefix} security file missing: {candidate}")
                continue
            if candidate.stat().st_size != security_file["length"]:
                errors.append(f"{prefix} security-file length drift: {candidate}")
            if sha256_file(candidate) != security_file["sha256"]:
                errors.append(f"{prefix} security-file SHA-256 drift: {candidate}")
    return errors


def validate_manifest(manifest: Any, repo_root: Path = REPO_ROOT) -> list[str]:
    errors: list[str] = []
    if not isinstance(manifest, dict):
        return ["manifest must be a JSON object"]

    if manifest.get("schema_version") != 1:
        errors.append("schema_version must equal 1")
    if not valid_https_url(manifest.get("issue_url")):
        errors.append("issue_url must be an absolute https URL")
    try:
        date.fromisoformat(manifest.get("observed_at", ""))
    except (TypeError, ValueError):
        errors.append("observed_at must use YYYY-MM-DD")
    if not is_nonempty_string(manifest.get("scope")):
        errors.append("scope must be a non-empty string")

    records = manifest.get("records")
    if not isinstance(records, list):
        return errors + ["records must be an array"]

    seen: set[str] = set()
    for index, record in enumerate(records):
        prefix = f"records[{index}]"
        if not isinstance(record, dict):
            errors.append(f"{prefix} must be an object")
            continue

        app_id = record.get("app_id")
        if not is_nonempty_string(app_id) or not app_id.isdecimal():
            errors.append(f"{prefix}.app_id must be a decimal string")
            app_id = f"invalid-{index}"
        elif app_id in seen:
            errors.append(f"{prefix}.app_id duplicates {app_id}")
        else:
            seen.add(app_id)

        if not is_nonempty_string(record.get("game")):
            errors.append(f"{prefix}.game must be a non-empty string")
        classification = record.get("classification")
        if classification not in ALLOWED_CLASSIFICATIONS:
            errors.append(
                f"{prefix}.classification must be one of {sorted(ALLOWED_CLASSIFICATIONS)}"
            )
        if not isinstance(record.get("final_for_observed_build"), bool):
            errors.append(f"{prefix}.final_for_observed_build must be boolean")
        elif classification in {"adapted", "not_adaptable"} and record.get(
            "final_for_observed_build"
        ) is not True:
            errors.append(
                f"{prefix}.final_for_observed_build must be true for {classification}"
            )
        if not isinstance(record.get("no_injection"), bool):
            errors.append(f"{prefix}.no_injection must be boolean")
        elif classification == "not_adaptable" and record.get("no_injection") is not True:
            errors.append(f"{prefix}.no_injection must be true for not_adaptable")
        if not is_nonempty_string(record.get("rationale")):
            errors.append(f"{prefix}.rationale must be a non-empty string")

        executable = record.get("executable")
        if not isinstance(executable, dict):
            errors.append(f"{prefix}.executable must be an object")
        else:
            if not is_nonempty_string(executable.get("relative_install_path")):
                errors.append(f"{prefix}.executable.relative_install_path is required")
            if not SHA256_RE.fullmatch(str(executable.get("sha256", ""))):
                errors.append(f"{prefix}.executable.sha256 must be lowercase SHA-256")
        if not SHA256_RE.fullmatch(str(record.get("appmanifest_sha256", ""))):
            errors.append(f"{prefix}.appmanifest_sha256 must be lowercase SHA-256")

        codes = record.get("exclusion_codes")
        if not isinstance(codes, list):
            errors.append(f"{prefix}.exclusion_codes must be an array")
            codes = []
        else:
            if classification == "adapted" and codes:
                errors.append(f"{prefix}.adapted records cannot have exclusion_codes")
            if classification in {"blocked", "not_adaptable"} and not codes:
                errors.append(
                    f"{prefix}.{classification} records require at least one exclusion code"
                )
            for code in codes:
                if not isinstance(code, str) or code not in ALLOWED_CODES:
                    errors.append(f"{prefix}.exclusion_codes contains unknown code {code!r}")
            if all(isinstance(code, str) for code in codes) and len(codes) != len(
                set(codes)
            ):
                errors.append(f"{prefix}.exclusion_codes contains duplicates")

        security_files = record.get("installed_security_files")
        if not isinstance(security_files, list):
            errors.append(f"{prefix}.installed_security_files must be an array")
            security_files = []
        for security_index, security_file in enumerate(security_files):
            security_prefix = f"{prefix}.installed_security_files[{security_index}]"
            if not isinstance(security_file, dict):
                errors.append(f"{security_prefix} must be an object")
                continue
            if not is_nonempty_string(security_file.get("relative_install_path")):
                errors.append(f"{security_prefix}.relative_install_path is required")
            if not isinstance(security_file.get("length"), int) or security_file["length"] <= 0:
                errors.append(f"{security_prefix}.length must be a positive integer")
            if not SHA256_RE.fullmatch(str(security_file.get("sha256", ""))):
                errors.append(f"{security_prefix}.sha256 must be lowercase SHA-256")

        evidence = record.get("evidence")
        if not isinstance(evidence, list) or len(evidence) < 2:
            errors.append(f"{prefix}.evidence must contain at least two entries")
            evidence = []
        evidence_kinds: set[str] = set()
        for evidence_index, item in enumerate(evidence):
            evidence_prefix = f"{prefix}.evidence[{evidence_index}]"
            if not isinstance(item, dict):
                errors.append(f"{evidence_prefix} must be an object")
                continue
            kind = item.get("kind")
            if kind not in ALLOWED_EVIDENCE_KINDS:
                errors.append(f"{evidence_prefix}.kind is not allowed")
            else:
                evidence_kinds.add(kind)

            facts = item.get("facts")
            if (
                not isinstance(facts, list)
                or not facts
                or any(not is_nonempty_string(fact) for fact in facts)
            ):
                errors.append(f"{evidence_prefix}.facts must be a non-empty string array")

            if isinstance(kind, str) and kind.startswith("official_store_"):
                url = item.get("url")
                if not valid_https_url(url):
                    errors.append(f"{evidence_prefix}.url must be an absolute https URL")
                elif f"/app/{app_id}/" not in url:
                    errors.append(f"{evidence_prefix}.url does not match AppID {app_id}")
            elif kind in {"repository_record", "repository_survey"}:
                repo_path = item.get("repo_path")
                if not is_nonempty_string(repo_path):
                    errors.append(f"{evidence_prefix}.repo_path is required")
                else:
                    candidate = (repo_root / repo_path).resolve()
                    try:
                        candidate.relative_to(repo_root.resolve())
                    except ValueError:
                        errors.append(f"{evidence_prefix}.repo_path escapes the repository")
                    else:
                        if not candidate.is_file():
                            errors.append(f"{evidence_prefix}.repo_path does not exist: {repo_path}")
                if not is_nonempty_string(item.get("locator")):
                    errors.append(f"{evidence_prefix}.locator is required")

        if "G1_KERNEL_ANTI_CHEAT" in codes:
            if "official_store_disclosure" not in evidence_kinds:
                errors.append(f"{prefix} G1 requires official_store_disclosure evidence")
            if "repository_record" not in evidence_kinds:
                errors.append(f"{prefix} G1 requires a repository_record snapshot")
            if len(security_files) < 2:
                errors.append(f"{prefix} G1 requires at least two hashed installation security files")
        if "G1_EMBEDDED_ANTI_TAMPER_NO_DISABLED_MODE" in codes:
            if "repository_record" not in evidence_kinds:
                errors.append(f"{prefix} embedded anti-tamper exclusion requires a repository_record snapshot")
            if len(security_files) < 1:
                errors.append(f"{prefix} embedded anti-tamper exclusion requires a hashed build-log or security file")
        if any(code.startswith("G8_") for code in codes):
            if "official_store_taxonomy" not in evidence_kinds and app_id != "1240440":
                errors.append(f"{prefix} G8 requires official_store_taxonomy evidence")
            if not evidence_kinds.intersection({"repository_record", "repository_survey"}):
                errors.append(f"{prefix} G8 requires repository evidence")
        if "OUT_OF_SCOPE_UTILITY_NOT_GAME" in codes:
            if "official_store_taxonomy" not in evidence_kinds:
                errors.append(f"{prefix} utility exclusion requires official_store_taxonomy evidence")

        reopen_if = record.get("reopen_if")
        if (
            not isinstance(reopen_if, list)
            or not reopen_if
            or any(not is_nonempty_string(condition) for condition in reopen_if)
        ):
            errors.append(f"{prefix}.reopen_if must be a non-empty string array")

    missing = sorted(EXPECTED_APP_IDS - seen)
    unexpected = sorted(seen - EXPECTED_APP_IDS)
    if missing:
        errors.append(f"missing expected AppIDs: {', '.join(missing)}")
    if unexpected:
        errors.append(f"unexpected AppIDs: {', '.join(unexpected)}")
    if len(records) != len(EXPECTED_APP_IDS):
        errors.append(
            f"records must contain exactly {len(EXPECTED_APP_IDS)} entries, got {len(records)}"
        )
    return errors


def run_self_test(manifest: dict[str, Any]) -> None:
    baseline = validate_manifest(manifest)
    if baseline:
        raise AssertionError(f"baseline manifest failed validation: {baseline}")

    tests: list[tuple[str, dict[str, Any], str]] = []

    missing = copy.deepcopy(manifest)
    missing["records"].pop()
    tests.append(("missing record", missing, "missing expected AppIDs"))

    weak_g1 = copy.deepcopy(manifest)
    weak_g1_record = next(
        record
        for record in weak_g1["records"]
        if "G1_KERNEL_ANTI_CHEAT" in record["exclusion_codes"]
    )
    weak_g1_record["installed_security_files"] = []
    tests.append(("G1 without files", weak_g1, "G1 requires at least two"))

    weak_embedded = copy.deepcopy(manifest)
    weak_embedded_record = next(
        record
        for record in weak_embedded["records"]
        if "G1_EMBEDDED_ANTI_TAMPER_NO_DISABLED_MODE" in record["exclusion_codes"]
    )
    weak_embedded_record["installed_security_files"] = []
    tests.append(
        (
            "embedded anti-tamper without build log",
            weak_embedded,
            "embedded anti-tamper exclusion requires a hashed build-log",
        )
    )

    inject_allowed = copy.deepcopy(manifest)
    inject_allowed["records"][0]["no_injection"] = False
    tests.append(("no-injection drift", inject_allowed, "no_injection must be true"))

    missing_repo_evidence = copy.deepcopy(manifest)
    missing_repo_evidence["records"][0]["evidence"][1][
        "repo_path"
    ] = "DataMiner/does-not-exist.json"
    tests.append(("missing repository evidence", missing_repo_evidence, "does not exist"))

    unknown_code = copy.deepcopy(manifest)
    unknown_code["records"][0]["exclusion_codes"].append("ASSUME_UNSAFE")
    tests.append(("unknown exclusion code", unknown_code, "unknown code"))

    invalid_evidence_kind = copy.deepcopy(manifest)
    invalid_evidence_kind["records"][0]["evidence"][0]["kind"] = None
    tests.append(
        ("invalid evidence kind", invalid_evidence_kind, "kind is not allowed")
    )

    invalid_classification = copy.deepcopy(manifest)
    invalid_classification["records"][0]["classification"] = "ready_enough"
    tests.append(
        ("invalid classification", invalid_classification, "classification must be one of")
    )

    for name, fixture, expected in tests:
        errors = validate_manifest(fixture)
        if not any(expected in error for error in errors):
            raise AssertionError(
                f"self-test {name!r} did not produce {expected!r}; errors={errors}"
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument(
        "--steamapps-root",
        type=Path,
        help="optionally verify installed executables, app manifests, and security files",
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    try:
        manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"ERROR: cannot load {args.manifest}: {exc}", file=sys.stderr)
        return 1

    errors = validate_manifest(manifest)
    if not errors and args.steamapps_root is not None:
        errors.extend(validate_installed(manifest, args.steamapps_root))
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1

    if args.self_test:
        try:
            run_self_test(manifest)
        except AssertionError as exc:
            print(f"ERROR: {exc}", file=sys.stderr)
            return 1

    report = {
        "status": "pass",
        "schema_version": 1,
        "record_count": len(manifest["records"]),
        "app_ids": sorted(record["app_id"] for record in manifest["records"]),
        "self_test": args.self_test,
        "installed_verified": args.steamapps_root is not None,
    }
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        suffixes = []
        if args.self_test:
            suffixes.append("8 negative self-tests")
        if args.steamapps_root is not None:
            suffixes.append("installed hashes")
        suffix = f" plus {', '.join(suffixes)}" if suffixes else ""
        print(
            f"PASS: {report['record_count']} final classifications validated{suffix}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
