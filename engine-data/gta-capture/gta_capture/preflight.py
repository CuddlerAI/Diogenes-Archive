"""Version, dependency, platform, calibration, and policy preflight."""

from __future__ import annotations

import json
import hashlib
import math
import os
import platform
import re
import shutil
import subprocess
import tarfile
import zipfile
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path, PureWindowsPath
from typing import Any

from .errors import ContractError
from .jsonio import load_json, sha256_file, utc_now
from .policy import CapturePolicy, repository_profile_non_trainable, repository_profile_path
from .controller_receipt import CONTROLLER_DERIVED_SCHEMA, validate_controller_receipt
from .replus_receipt import (REPLUS_DERIVED_SCHEMA, is_reviewed_replus_patch,
                             validate_replus_receipt)
from .controller_replus_receipt import (CONTROLLER_REPLUS_DERIVED_SCHEMA,
                                        CONTROLLER_REPLUS_KIND,
                                        validate_controller_replus_receipt)


MANIFEST_SCHEMA = "dm-gta-dependency-manifest/1"
PROJECTION_SCHEMA = "dm-gta-projection-calibration/1"
CAPTURE_BUILD_SCHEMA = "dm-gta-capture-build-receipt/1"
RESHADE_SETUP_SHA256 = "afe4c8f13048306307983b8b3d41d5bf00a86820440b0e57dea10950e1176445"
RESHADE_SIGNER = "589690208A5E52FB96980C4A6698F50ACD47C49F"
REPLUS_RENDER_PROFILE_SCHEMA = "dm-gta-replus-render-profile/1"
REPLUS_RENDER_PROFILE_ALGORITHM = "replace-unique-keys-utf8-crlf/1"
REPLUS_RENDER_PROFILE = {
    "EnableRenderer": "1",
    "RenderCaptureMode": "Walking",
    "RenderDepthOfField": "0",
    "RenderFps": "30",
    "RenderSamples": "1",
    "RenderHighlightBoost": "0.9",
    "RenderSettleFrames": "3",
    "RenderSettleSubFrames": "1",
    "RenderJpeg": "0",
    "RenderHideHud": "1",
    "ExportCloseWhenDone": "1",
    "RenderMode": "Frames",
    "RenderKeepFrames": "1",
    "RenderAudio": "1",
    "RenderMarkerSpeed": "1",
}


@dataclass(frozen=True)
class ProjectionValidation:
    calibration_id: str
    filmback_id: str
    metric_scale: float
    reprojection_error_px: float
    reprojection_threshold_px: float
    sample_count: int
    minimum_sample_count: int
    evidence_sha256: str


@dataclass(frozen=True)
class Check:
    check_id: str
    status: str
    summary: str
    evidence: dict | None = None

    def to_dict(self) -> dict:
        value = {"id": self.check_id, "status": self.status, "summary": self.summary}
        if self.evidence is not None:
            value["evidence"] = self.evidence
        return value


@dataclass
class PreflightReport:
    mode: str
    checks: list[Check]

    @property
    def ok(self) -> bool:
        return all(item.status != "FAIL" for item in self.checks)

    def to_dict(self) -> dict:
        return {
            "schema": "dm-gta-preflight/1",
            "created_at": utc_now(),
            "mode": self.mode,
            "ok": self.ok,
            "checks": [item.to_dict() for item in self.checks],
        }


@dataclass(frozen=True)
class DependencyManifest:
    path: Path
    raw: dict
    policy: CapturePolicy

    @classmethod
    def load(cls, path: os.PathLike[str] | str) -> "DependencyManifest":
        source = Path(path)
        raw = load_json(source)
        if not isinstance(raw, dict) or raw.get("schema") != MANIFEST_SCHEMA:
            raise ContractError(f"manifest.schema must equal {MANIFEST_SCHEMA!r}")
        for key in ("game", "gpu", "components", "source_pins", "render",
                    "calibration", "policy"):
            if not isinstance(raw.get(key), dict):
                raise ContractError(f"manifest.{key} must be an object")
        policy = CapturePolicy.from_dict(raw["policy"])
        return cls(source.resolve(), raw, policy)

    def resolve_path(self, value: str) -> Path:
        expanded = os.path.expandvars(os.path.expanduser(value))
        path = Path(expanded)
        if not path.is_absolute():
            path = self.path.parent / path
        return path.resolve()

    def component_path(self, name: str) -> Path | None:
        item = self.raw["components"].get(name)
        if not isinstance(item, dict):
            return None
        value = item.get("path")
        if not isinstance(value, str) or not value.strip():
            return None
        if item.get("allow_path_lookup"):
            found = shutil.which(value)
            return Path(found).resolve() if found else None
        return self.resolve_path(value)

    def pinned_tool_path(self, name: str) -> str:
        """Resolve one pinned executable; never fall back to an unpinned PATH hit."""
        path = self.component_path(name)
        if path is None or not path.is_file():
            raise ContractError(f"pinned {name} is unavailable")
        return str(path)


@dataclass(frozen=True)
class RenderProfileBinding:
    expected_output_root: str
    render_profile_sha256: str


def load_render_profile_binding(manifest: DependencyManifest) -> RenderProfileBinding:
    """Load the exact deployed Render.ini identity used by the native plan.

    The manifest pins the receipt, the receipt pins the canonical Render.ini
    transform, and the installed Render.ini must still have that same digest.
    Calibration may remain fail-closed while this narrower runtime binding is
    prepared, so this deliberately does not treat a full preflight PASS as a
    prerequisite.
    """
    receipt_item = manifest.raw["components"].get("capture_build_receipt")
    render_item = manifest.raw["components"].get("replus_render_ini")
    if not isinstance(receipt_item, dict) or not isinstance(render_item, dict):
        raise ContractError("manifest must pin capture_build_receipt and replus_render_ini")
    receipt_path = manifest.component_path("capture_build_receipt")
    render_path = manifest.component_path("replus_render_ini")
    for label, item, path in (
            ("capture build receipt", receipt_item, receipt_path),
            ("RE+ Render.ini", render_item, render_path)):
        expected = item.get("sha256")
        if (path is None or not path.is_file() or not isinstance(expected, str)
                or not re.fullmatch(r"[0-9a-fA-F]{64}", expected)
                or sha256_file(path) != expected.lower()):
            raise ContractError(f"{label} is absent or differs from its manifest pin")
    receipt = load_json(receipt_path)
    if isinstance(receipt, dict) and receipt.get("schema") == CONTROLLER_REPLUS_DERIVED_SCHEMA:
        receipt = validate_controller_replus_receipt(
            manifest, receipt, validate_parent=_check_capture_build_receipt_v1)
    if isinstance(receipt, dict) and receipt.get("schema") == REPLUS_DERIVED_SCHEMA:
        receipt = validate_replus_receipt(
            manifest, receipt, validate_parent=_check_capture_build_receipt_v1)
    if isinstance(receipt, dict) and receipt.get("schema") == CONTROLLER_DERIVED_SCHEMA:
        receipt = validate_controller_receipt(
            manifest, receipt, validate_parent=_check_capture_build_receipt_v1)
    if not isinstance(receipt, dict) or receipt.get("schema") != CAPTURE_BUILD_SCHEMA:
        raise ContractError("capture build receipt schema mismatch")
    transforms = receipt.get("configuration_transforms")
    transform = transforms.get("replus_render_ini") if isinstance(transforms, dict) else None
    artifacts = receipt.get("artifacts")
    artifact = artifacts.get("replus_render_ini") if isinstance(artifacts, dict) else None
    if (not isinstance(transform, dict)
            or transform.get("schema") != REPLUS_RENDER_PROFILE_SCHEMA
            or transform.get("algorithm") != REPLUS_RENDER_PROFILE_ALGORITHM
            or not isinstance(transform.get("settings"), dict)
            or not isinstance(transform.get("output"), dict)
            or not isinstance(artifact, dict)):
        raise ContractError("capture build receipt has no sealed RE+ render profile")
    settings = transform["settings"]
    output_root = settings.get("RenderOutputFolder")
    if not isinstance(output_root, str) or not output_root.strip():
        raise ContractError("real export requires a non-empty RenderOutputFolder")
    expected_settings = dict(REPLUS_RENDER_PROFILE)
    expected_settings["RenderOutputFolder"] = output_root
    if settings != expected_settings:
        raise ContractError("capture build receipt render profile differs from the fixed contract")
    digest = transform["output"].get("sha256")
    if (not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest)
            or digest != artifact.get("sha256") or digest != render_item.get("sha256")):
        raise ContractError("Render.ini digest chain differs across receipt and manifest")
    root_path = Path(os.path.expandvars(os.path.expanduser(output_root)))
    if not root_path.is_absolute():
        raise ContractError("RenderOutputFolder must be absolute for a real export")
    return RenderProfileBinding(str(root_path.resolve()), digest)


def _component_path(manifest: DependencyManifest, item: dict) -> Path | None:
    value = item.get("path")
    if not isinstance(value, str) or not value.strip():
        return None
    if item.get("allow_path_lookup"):
        found = shutil.which(value)
        return Path(found).resolve() if found else None
    return manifest.resolve_path(value)


def _check_file(check_id: str, manifest: DependencyManifest, item: Any,
                *, require_pin: bool) -> list[Check]:
    if not isinstance(item, dict):
        return [Check(check_id, "FAIL", "component entry is missing or not an object")]
    path = _component_path(manifest, item)
    if path is None or not path.is_file():
        return [Check(check_id, "FAIL", f"file not found: {item.get('path', '')}")]
    actual = sha256_file(path)
    expected = item.get("sha256", "")
    if require_pin and (not isinstance(expected, str) or len(expected) != 64):
        return [Check(check_id, "FAIL", "sha256 pin is required for real capture",
                      {"path": str(path), "actual_sha256": actual})]
    if expected and actual.lower() != str(expected).lower():
        return [Check(check_id, "FAIL", "sha256 mismatch",
                      {"path": str(path), "expected": expected, "actual": actual})]
    status = "PASS" if expected else "WARN"
    summary = "file exists and hash matches" if expected else "file exists but is not pinned"
    return [Check(check_id, status, summary, {"path": str(path), "sha256": actual})]


def _windows_powershell_path() -> str | None:
    if platform.system() != "Windows":
        return None
    # Some capture hosts expose only the canonical *.exe name through PATH.
    # Never use a shell string: resolve one executable and pass it directly.
    for name in ("pwsh.exe", "pwsh", "powershell.exe", "powershell"):
        found = shutil.which(name)
        if found:
            return found
    system_root = os.environ.get("SystemRoot")
    if system_root:
        fallback = Path(system_root) / "System32" / "WindowsPowerShell" / "v1.0" / "powershell.exe"
        if fallback.is_file():
            return str(fallback)
    return None


def _run_windows_powershell_json(script: str, *, timeout: int,
                                 path: Path | None = None) -> Any:
    powershell = _windows_powershell_path()
    if powershell is None:
        return None
    environment = os.environ.copy()
    if path is not None:
        # Avoid -Command argument binding and quoting differences between
        # Windows PowerShell 5.1 and pwsh. The subprocess receives a private
        # environment copy, and every probe still uses -LiteralPath.
        environment["DIOGENES_CAPTURE_PROBE_PATH"] = str(path)
    command = [
        powershell, "-NoProfile", "-NonInteractive", "-Command",
        "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); " + script,
    ]
    try:
        proc = subprocess.run(
            command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace", timeout=timeout,
            env=environment,
        )
        stdout = proc.stdout.lstrip("\ufeff").strip()
        if proc.returncode != 0 or not stdout:
            return None
        return json.loads(stdout)
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError):
        return None


def _is_absolute_executable_path(value: Any) -> bool:
    return (isinstance(value, str)
            and (Path(value).is_absolute() or PureWindowsPath(value).is_absolute()))


def _actual_windows_file_version(path: Path) -> str | None:
    if platform.system() != "Windows":
        return None
    value = _run_windows_powershell_json(
        "$v=(Get-Item -LiteralPath $env:DIOGENES_CAPTURE_PROBE_PATH).VersionInfo; "
        "[pscustomobject]@{file_major=[int]$v.FileMajorPart;"
        "file_minor=[int]$v.FileMinorPart;file_build=[int]$v.FileBuildPart;"
        "file_private=[int]$v.FilePrivatePart;file_version=[string]$v.FileVersion;"
        "product_version=[string]$v.ProductVersion}|ConvertTo-Json -Compress",
        timeout=15, path=path,
    )
    if not isinstance(value, dict):
        return None
    parts = [value.get(name) for name in
             ("file_major", "file_minor", "file_build", "file_private")]
    if all(isinstance(part, int) and not isinstance(part, bool) and part >= 0
           for part in parts):
        return ".".join(str(part) for part in parts)
    for key in ("file_version", "product_version"):
        raw = value.get(key)
        if not isinstance(raw, str):
            continue
        match = re.search(r"(?<!\d)\d+\.\d+\.\d+\.\d+(?!\d)", raw)
        if match:
            return match.group(0)
    return None


def _actual_windows_gpu_inventory() -> list[dict[str, str]]:
    if platform.system() != "Windows":
        return []
    value = _run_windows_powershell_json(
        "Get-CimInstance Win32_VideoController | ForEach-Object { "
        "[pscustomobject]@{adapter_name=$_.Name;driver_version=$_.DriverVersion;"
        "pnp_device_id=$_.PNPDeviceID} } | ConvertTo-Json -Compress",
        timeout=20,
    )
    if value is None:
        return []
    rows = value if isinstance(value, list) else [value]
    result = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        result.append({
            "adapter_name": str(row.get("adapter_name") or "").strip(),
            "driver_version": str(row.get("driver_version") or "").strip(),
            "pnp_device_id": str(row.get("pnp_device_id") or "").strip(),
        })
    return result


def _actual_windows_authenticode(path: Path) -> dict[str, Any] | None:
    if platform.system() != "Windows":
        return None
    value = _run_windows_powershell_json(
        "$s=Get-AuthenticodeSignature -LiteralPath $env:DIOGENES_CAPTURE_PROBE_PATH; "
        "$c=New-Object Security.Cryptography.X509Certificates.X509Chain; "
        "if($s.SignerCertificate){$null=$c.Build($s.SignerCertificate)}; "
        "[pscustomobject]@{status=[string]$s.Status;thumbprint="
        "[string]$s.SignerCertificate.Thumbprint;chain_status="
        "@($c.ChainStatus|ForEach-Object{[string]$_.Status})}|ConvertTo-Json -Compress",
        timeout=20, path=path,
    )
    if not isinstance(value, dict):
        return None
    chain_status = value.get("chain_status", [])
    if isinstance(chain_status, str):
        chain_status = [chain_status]
    if not isinstance(chain_status, list):
        chain_status = []
    return {"status": str(value.get("status") or ""),
            "thumbprint": str(value.get("thumbprint") or "").upper(),
            "chain_status": [str(item) for item in chain_status]}


def _canonical_reshade_signature(signature: dict[str, Any] | None) -> dict | None:
    if not isinstance(signature, dict):
        return None
    chain = signature.get("chain_status")
    if isinstance(chain, str):
        chain = [chain]
    if (not isinstance(chain, list)
            or any(not isinstance(item, str) or not item.strip() for item in chain)):
        return None
    return {
        "status": str(signature.get("status") or "").strip(),
        "thumbprint": str(signature.get("thumbprint") or "").strip().upper(),
        "chain_status": [item.strip() for item in chain],
    }


def _approved_reshade_signature(signature: dict[str, Any] | None) -> bool:
    canonical = _canonical_reshade_signature(signature)
    if canonical is None or canonical["thumbprint"] != RESHADE_SIGNER:
        return False
    status = canonical["status"]
    chain = canonical["chain_status"]
    return ((status == "Valid" and chain == [])
            or (status == "UnknownError" and chain == ["UntrustedRoot"]))


def _approved_game_root_asi_names(manifest: DependencyManifest,
                                  game_path: Path | None) -> set[str]:
    """Return only ASIs that are both contract-approved and pinned in this game root."""
    allowed: set[str] = set()
    if game_path is None:
        return allowed
    expected = {
        "replus_asi": "rockstareditorplus.asi",
        "diogenes_controller_asi": "diogenesgtacontroller.asi",
        "reshade_dxgi": "reshade64.asi",
    }
    game_root = os.path.normcase(str(game_path.parent))
    for component, expected_name in expected.items():
        component_path = manifest.component_path(component)
        if (component_path is not None
                and os.path.normcase(str(component_path.parent)) == game_root
                and component_path.name.casefold() == expected_name):
            allowed.add(expected_name)
    return allowed


def _is_approved_id(value: Any) -> bool:
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:-]{2,127}", value):
        return False
    lowered = value.casefold()
    return not any(marker in lowered for marker in ("pending", "unknown", "unapproved", "todo"))


def _check_gpu_driver(manifest: DependencyManifest) -> Check:
    expected = manifest.raw["gpu"]
    expected_name = expected.get("adapter_name")
    expected_driver = expected.get("driver_version")
    expected_pnp = expected.get("pnp_device_id")
    inventory = _actual_windows_gpu_inventory()
    expected_ok = (
        expected.get("vendor") == "NVIDIA"
        and isinstance(expected_name, str) and bool(expected_name.strip())
        and "pending" not in expected_name.casefold()
        and isinstance(expected_driver, str) and bool(expected_driver.strip())
        and "pending" not in expected_driver.casefold()
        and (expected_pnp is None or (isinstance(expected_pnp, str) and bool(expected_pnp.strip())))
    )
    matches = [row for row in inventory
               if row["adapter_name"] == expected_name
               and row["driver_version"] == expected_driver
               and "VEN_10DE" in row["pnp_device_id"].upper()
               and (expected_pnp is None or row["pnp_device_id"] == expected_pnp)]
    ok = expected_ok and len(matches) == 1
    return Check(
        "environment.gpu_driver_lock", "PASS" if ok else "FAIL",
        "actual NVIDIA adapter and Windows driver version match the approved manifest"
        if ok else "actual GPU/driver does not match one unambiguous approved NVIDIA lock",
        {"expected": expected, "actual_inventory": inventory, "matching_adapters": matches},
    )


def _evaluate_projection_calibration(calibration: dict, path: Path | None,
                                     *, expected_render: dict | None = None,
                                     expected_game_build: str | None = None
                                     ) -> tuple[ProjectionValidation | None, dict]:
    item = calibration.get("projection_evidence")
    expected_sha = item.get("sha256") if isinstance(item, dict) else None
    if (path is None or not path.is_file() or not isinstance(expected_sha, str)
            or len(expected_sha) != 64 or sha256_file(path).casefold() != expected_sha.casefold()):
        return None, {"problems": [
            "projection evidence must exist and match its approved SHA-256 pin"
        ]}
    try:
        evidence = load_json(path)
    except (OSError, UnicodeError, json.JSONDecodeError, ContractError) as exc:
        return None, {"path": str(path), "problems": [
            f"cannot parse projection evidence: {exc}"
        ]}
    if not isinstance(evidence, dict):
        return None, {"path": str(path), "problems": [
            "projection evidence root must be an object"
        ]}

    acceptance = calibration.get("acceptance")
    reprojection = evidence.get("reprojection")
    render_evidence = evidence.get("render")
    max_error = acceptance.get("max_reprojection_error_px") if isinstance(acceptance, dict) else None
    min_samples = acceptance.get("min_sample_count") if isinstance(acceptance, dict) else None
    actual_error = reprojection.get("error_px") if isinstance(reprojection, dict) else None
    actual_samples = reprojection.get("sample_count") if isinstance(reprojection, dict) else None
    metric_scale = calibration.get("metric_scale")
    evidence_scale = evidence.get("metric_scale")
    numeric = lambda value: isinstance(value, (int, float)) and not isinstance(value, bool)
    problems = []
    if evidence.get("schema") != PROJECTION_SCHEMA:
        problems.append(f"schema must equal {PROJECTION_SCHEMA}")
    if evidence.get("approval_status") != "approved":
        problems.append("approval_status must equal approved")
    for key in ("calibration_id", "filmback_id"):
        expected = calibration.get(key)
        actual = evidence.get(key)
        if not _is_approved_id(expected) or actual != expected:
            problems.append(f"approved {key} is missing or does not match the manifest")
    if calibration.get("metric_scale_approved") is not True:
        problems.append("metric_scale_approved must be true")
    if (not numeric(metric_scale) or not numeric(evidence_scale)
            or not math.isfinite(float(metric_scale)) or float(metric_scale) <= 0
            or not math.isclose(float(metric_scale), float(evidence_scale), rel_tol=1e-9,
                                abs_tol=1e-12)):
        problems.append("evidence metric_scale must be finite, positive, and match the manifest")
    if (not numeric(max_error) or not math.isfinite(float(max_error)) or float(max_error) <= 0
            or not numeric(actual_error) or not math.isfinite(float(actual_error))
            or float(actual_error) < 0 or float(actual_error) > float(max_error)):
        problems.append("reprojection error is missing or exceeds the approved pixel threshold")
    if (not isinstance(min_samples, int) or isinstance(min_samples, bool) or min_samples < 1
            or not isinstance(actual_samples, int) or isinstance(actual_samples, bool)
            or actual_samples < min_samples):
        problems.append("reprojection sample_count is below the approved minimum")
    if not isinstance(reprojection, dict) or reprojection.get("metric") != "rms_euclidean_px":
        problems.append("reprojection metric must equal rms_euclidean_px")
    render_matches = (expected_render is None or (
        isinstance(render_evidence, dict)
        and render_evidence.get("width") == expected_render.get("width")
        and render_evidence.get("height") == expected_render.get("height")
        and render_evidence.get("graphics_api") == expected_render.get("graphics_api")
    ))
    build_matches = (expected_game_build is None
                     or evidence.get("game_build") == expected_game_build)
    if not render_matches or not build_matches:
        problems.append("projection evidence build/resolution/graphics API differs from the manifest")

    details = {
        "path": str(path), "sha256": expected_sha, "problems": problems,
        "calibration_id": evidence.get("calibration_id"),
        "filmback_id": evidence.get("filmback_id"),
        "metric_scale": evidence_scale, "reprojection": reprojection,
        "acceptance": acceptance, "render": render_evidence,
        "game_build": evidence.get("game_build"),
    }
    if problems:
        return None, details
    validation = ProjectionValidation(
        calibration_id=str(evidence["calibration_id"]),
        filmback_id=str(evidence["filmback_id"]),
        metric_scale=float(evidence_scale),
        reprojection_error_px=float(actual_error),
        reprojection_threshold_px=float(max_error),
        sample_count=int(actual_samples),
        minimum_sample_count=int(min_samples),
        evidence_sha256=expected_sha.casefold(),
    )
    return validation, details


def load_projection_validation(calibration: dict) -> ProjectionValidation | None:
    """Re-validate an absolute, hash-pinned projection artifact at the final gate."""
    item = calibration.get("projection_evidence")
    value = item.get("path") if isinstance(item, dict) else None
    if not isinstance(value, str) or not value.strip():
        return None
    expanded = Path(os.path.expandvars(os.path.expanduser(value)))
    if not expanded.is_absolute():
        return None
    validation, _ = _evaluate_projection_calibration(calibration, expanded.resolve())
    return validation


def _check_projection_calibration(manifest: DependencyManifest,
                                  calibration: dict) -> Check:
    item = calibration.get("projection_evidence")
    raw_path = item.get("path") if isinstance(item, dict) else None
    if (not isinstance(raw_path, str)
            or not Path(os.path.expandvars(os.path.expanduser(raw_path))).is_absolute()):
        return Check("calibration.projection_contract", "FAIL",
                     "projection evidence path must be absolute so the sealed gate reopens the same file")
    path = _component_path(manifest, item) if isinstance(item, dict) else None
    validation, details = _evaluate_projection_calibration(
        calibration, path, expected_render=manifest.raw["render"],
        expected_game_build=manifest.raw["game"].get("expected_build"),
    )
    if validation is None:
        return Check("calibration.projection_contract", "FAIL",
                     "projection calibration evidence violates the approved contract", details)
    return Check("calibration.projection_contract", "PASS",
                 "approved projection IDs, metric scale, sample count, and error threshold match",
                 details)


def _flat_ini(path: Path) -> dict[str, str]:
    result = {}
    for raw in path.read_text(encoding="utf-8-sig", errors="strict").splitlines():
        line = raw.strip()
        if not line or line.startswith((";", "#", "[")) or "=" not in line:
            continue
        key, value = line.split("=", 1)
        result[key.strip().casefold()] = value.strip()
    return result


def _check_render_files(manifest: DependencyManifest) -> list[Check]:
    checks = []
    render = manifest.raw["render"]
    render_ini = manifest.component_path("replus_render_ini")
    expected_ini = {
        "enablerenderer": "1", "rendercapturemode": "Walking",
        "renderdepthoffield": "0", "renderfps": "30", "rendersamples": "1",
        "renderjpeg": "0", "renderhidehud": "1", "rendermode": "Frames",
        "renderkeepframes": "1", "renderaudio": "1", "rendermarkerspeed": "1",
        # The v18 fence assumes RE+ never issues request N+1 before the observer
        # acks N; settle frames >= 1 is that precondition, deploy pins 3.
        "rendersettleframes": "3",
    }
    if render_ini is None or not render_ini.is_file():
        checks.append(Check("render.replus_ini", "FAIL", "RE+ Render.ini is unavailable"))
    else:
        try:
            actual_ini = _flat_ini(render_ini)
            wrong = {key: {"expected": value, "actual": actual_ini.get(key)}
                     for key, value in expected_ini.items() if actual_ini.get(key) != value}
            checks.append(Check("render.replus_ini", "FAIL" if wrong else "PASS",
                                "RE+ frame/audio settings mismatch" if wrong else
                                "RE+ frame/audio settings match the POC contract", wrong or expected_ini))
        except (OSError, UnicodeError) as exc:
            checks.append(Check("render.replus_ini", "FAIL", f"cannot parse Render.ini: {exc}"))

    settings = manifest.component_path("gta_settings_xml")
    if settings is None or not settings.is_file():
        checks.append(Check("render.gta_settings", "FAIL", "GTA settings.xml is unavailable"))
    else:
        try:
            root = ET.parse(settings).getroot()
            values = {node.tag.casefold(): node.attrib.get("value") for node in root.iter()}
            expected = {
                "screenwidth": str(render.get("width")),
                "screenheight": str(render.get("height")),
                "dx_version": "2",
            }
            wrong = {key: {"expected": value, "actual": values.get(key)}
                     for key, value in expected.items() if values.get(key) != value}
            checks.append(Check("render.gta_settings", "FAIL" if wrong else "PASS",
                                "GTA output/DX settings mismatch" if wrong else
                                "GTA output is 1920x1080 DX11", wrong or expected))
        except (OSError, ET.ParseError) as exc:
            checks.append(Check("render.gta_settings", "FAIL", f"cannot parse settings.xml: {exc}"))
    return checks


def _check_reshade_receipt(manifest: DependencyManifest, game_path: Path | None) -> Check:
    receipt_path = manifest.component_path("reshade_install_receipt")
    dxgi_path = manifest.component_path("reshade_dxgi")
    problems = []
    try:
        receipt = load_json(receipt_path) if receipt_path is not None else None
    except (OSError, UnicodeError, json.JSONDecodeError, ContractError):
        receipt = None
    if not isinstance(receipt, dict) or receipt.get("schema") != "dm-gta-reshade-install-receipt/1":
        problems.append("receipt schema is missing")
        receipt = {}
    installer = receipt.get("installer") if isinstance(receipt.get("installer"), dict) else {}
    target = receipt.get("target") if isinstance(receipt.get("target"), dict) else {}
    installed = (receipt.get("installed_dxgi")
                 if isinstance(receipt.get("installed_dxgi"), dict) else {})
    payload = receipt.get("payload") if isinstance(receipt.get("payload"), dict) else {}
    receipt_signature = {
        "status": installer.get("signature_status"),
        "thumbprint": installer.get("signer_thumbprint"),
        "chain_status": installer.get("signature_chain_status", []),
    }
    canonical_receipt_signature = _canonical_reshade_signature(receipt_signature)
    if (installer.get("sha256") != RESHADE_SETUP_SHA256
            or installer.get("signer_thumbprint") != RESHADE_SIGNER
            or not _approved_reshade_signature(receipt_signature)):
        problems.append("receipt installer hash/signer is not the approved ReShade 6.8.0 build")
    normalize = lambda value: os.path.normcase(str(Path(str(value)).resolve()))
    if game_path is None or normalize(target.get("path", "")) != normalize(game_path):
        problems.append("receipt target path differs from GTA5.exe")
    if dxgi_path is None or normalize(installed.get("path", "")) != normalize(dxgi_path):
        problems.append("receipt installed_dxgi path differs from the pinned component")
    if (target.get("sha256") != manifest.raw["game"].get("sha256")
            or installed.get("sha256") != manifest.raw["components"].get(
                "reshade_dxgi", {}).get("sha256")):
        problems.append("receipt target/dxgi hashes differ from the manifest")
    installer_path_value = installer.get("path")
    installer_path = (Path(str(installer_path_value)).resolve()
                      if isinstance(installer_path_value, str) and installer_path_value else None)
    if installer_path is None or not installer_path.is_file():
        problems.append("receipt-bound ReShade installer is missing")
        signature = None
    else:
        if sha256_file(installer_path) != RESHADE_SETUP_SHA256:
            problems.append("receipt-bound ReShade installer hash changed")
        signature = _actual_windows_authenticode(installer_path)
        if (not _approved_reshade_signature(signature)
                or _canonical_reshade_signature(signature) != canonical_receipt_signature):
            problems.append("receipt-bound ReShade installer Authenticode signature is invalid")
        try:
            with zipfile.ZipFile(installer_path) as archive:
                embedded = archive.read("ReShade64.dll")
        except (OSError, KeyError, zipfile.BadZipFile):
            embedded = b""
            problems.append("approved installer has no readable ReShade64.dll payload")
        embedded_sha = hashlib.sha256(embedded).hexdigest()
        if (payload.get("archive_entry") != "ReShade64.dll"
                or payload.get("bytes") != len(embedded)
                or payload.get("sha256") != embedded_sha):
            problems.append("receipt payload differs from ReShade64.dll embedded in installer")
        if (dxgi_path is None or not dxgi_path.is_file()
                or dxgi_path.stat().st_size != len(embedded)
                or sha256_file(dxgi_path) != embedded_sha):
            problems.append("installed dxgi.dll is not the embedded approved ReShade64.dll")
    reshade_component_sha = manifest.raw["components"].get("reshade_dxgi", {}).get("sha256")
    if manifest.raw.get("source_pins", {}).get("reshade_dll_sha256") != reshade_component_sha:
        problems.append("source_pins.reshade_dll_sha256 differs from the installed component")
    return Check(
        "dependencies.reshade_install_receipt", "FAIL" if problems else "PASS",
        "signed installer receipt binds ReShade to this GTA executable"
        if not problems else "ReShade install receipt is not bound to this capture stack",
        {"path": str(receipt_path) if receipt_path else None,
         "installer_path": str(installer_path) if installer_path else None,
         "authenticode": signature, "problems": problems},
    )


def _check_capture_build_receipt(manifest: DependencyManifest) -> Check:
    path = manifest.component_path("capture_build_receipt")
    try:
        receipt = load_json(path) if path is not None else None
        if isinstance(receipt, dict) and receipt.get("schema") == CONTROLLER_REPLUS_DERIVED_SCHEMA:
            validate_controller_replus_receipt(
                manifest, receipt, validate_parent=_check_capture_build_receipt_v1)
            return Check("dependencies.capture_build_receipt", "PASS",
                         "controller-only snapshot build and unchanged RE+-v3/v2/v1 parent stack are bound",
                         {"path": str(path), "kind": CONTROLLER_REPLUS_KIND,
                          "reviewed_replus_patch_sha256": manifest.raw["source_pins"]["replus_patch_sha256"]})
        if isinstance(receipt, dict) and receipt.get("schema") == REPLUS_DERIVED_SCHEMA:
            validate_replus_receipt(
                manifest, receipt, validate_parent=_check_capture_build_receipt_v1)
            return Check("dependencies.capture_build_receipt", "PASS",
                         "RE+-only snapshot build and unchanged controller-v2/v1 parent stack are bound",
                         {"path": str(path), "kind": "replus_only_derived",
                          "reviewed_replus_patch_sha256": receipt["source_provenance"]["exact_patch"]["sha256"]})
        if isinstance(receipt, dict) and receipt.get("schema") == CONTROLLER_DERIVED_SCHEMA:
            validate_controller_receipt(
                manifest, receipt, validate_parent=_check_capture_build_receipt_v1)
            return Check("dependencies.capture_build_receipt", "PASS",
                         "controller-only snapshot build and unchanged v1 parent stack are bound",
                         {"path": str(path), "kind": "controller_only_derived"})
    except (OSError, UnicodeError, ValueError, TypeError, KeyError, ContractError,
            subprocess.SubprocessError, zipfile.BadZipFile, tarfile.TarError, ET.ParseError) as exc:
        return Check("dependencies.capture_build_receipt", "FAIL",
                     "capture build provenance is incomplete or mismatched",
                     {"path": str(path) if path else None, "problems": [str(exc)]})
    return _check_capture_build_receipt_v1(manifest)


def _check_capture_build_receipt_v1(manifest: DependencyManifest) -> Check:
    receipt_path = manifest.component_path("capture_build_receipt")
    problems: list[str] = []
    try:
        receipt = load_json(receipt_path) if receipt_path is not None else None
    except (OSError, UnicodeError, json.JSONDecodeError, ContractError):
        receipt = None
    if not isinstance(receipt, dict) or receipt.get("schema") != CAPTURE_BUILD_SCHEMA:
        problems.append("capture build receipt schema is missing")
        receipt = {}
    sources = receipt.get("sources") if isinstance(receipt.get("sources"), dict) else {}
    expected_sources = {
        "replus_commit": "c6eba57160ca444817b149b3304ed1454e0ba2d9",
        "replus_patch_sha256":
            "4582b99830b47f6816b16ccf16dec274d90d9c822430be3f482c3f610cad03aa",
        "igcs_fork_commit": "13c92ef2565403ffcc2501f779ca7cd935e5d598",
        "igcs_patch_sha256":
            "5acb2d375748a0ee91ee7fc0c5f0f38e349139dfff0e78963c4ace89f524d4f5",
    }
    source_pins = manifest.raw.get("source_pins", {})
    for key, expected in expected_sources.items():
        if sources.get(key) != expected or source_pins.get(key) != expected:
            problems.append(f"{key} differs across approved source, receipt, and manifest")
    controller_commit = sources.get("controller_git_commit")
    if (not isinstance(controller_commit, str)
            or re.fullmatch(r"[0-9a-f]{40}", controller_commit) is None
            or source_pins.get("controller_git_commit") != controller_commit):
        problems.append("controller_git_commit is absent or not bound to the manifest")
    toolchain = receipt.get("toolchain") if isinstance(receipt.get("toolchain"), dict) else {}
    invocations = toolchain.get("invocations")
    cmake_path = toolchain.get("cmake_path")
    msbuild_path = toolchain.get("msbuild_path")
    invocations_ok = (
        _is_absolute_executable_path(cmake_path)
        and _is_absolute_executable_path(msbuild_path)
        and isinstance(invocations, list) and len(invocations) == 3
        and all(isinstance(item, dict) for item in invocations)
        and invocations[0].get("executable") == cmake_path
        and invocations[0].get("argv", [])[:1] == ["--build"]
        and len(invocations[0].get("argv", [])) == 7
        and isinstance(invocations[0]["argv"][1], str)
        and _is_absolute_executable_path(invocations[0]["argv"][1])
        and invocations[0]["argv"][2:] == [
            "--config", "Release", "--clean-first", "--parallel", "4"]
        and invocations[1].get("executable") == msbuild_path
        and len(invocations[1].get("argv", [])) == 6
        and isinstance(invocations[1]["argv"][0], str)
        and _is_absolute_executable_path(invocations[1]["argv"][0])
        and invocations[1]["argv"][0].casefold().endswith("igcsconnector.sln")
        and invocations[1]["argv"][1:] == [
            "/t:Rebuild", "/m", "/p:Configuration=Release", "/p:Platform=x64", "/nologo"]
        and invocations[2].get("executable") == cmake_path
        and invocations[2].get("argv", [])[:1] == ["--build"]
        and len(invocations[2].get("argv", [])) == 7
        and isinstance(invocations[2]["argv"][1], str)
        and _is_absolute_executable_path(invocations[2]["argv"][1])
        and invocations[2]["argv"][2:] == [
            "--config", "Release", "--clean-first", "--parallel", "4"]
    )
    if invocations_ok:
        expected_command_strings = [
            " ".join([item["executable"], *item["argv"]]) for item in invocations
        ]
        invocations_ok = toolchain.get("commands") == expected_command_strings
    if (not isinstance(receipt.get("started_at"), str)
            or not isinstance(receipt.get("finished_at"), str)):
        problems.append("clean rebuild start/finish timestamps are absent")
    if (not isinstance(toolchain.get("cmake_version"), str)
            or not toolchain["cmake_version"].strip()
            or not isinstance(toolchain.get("msbuild_version"), str)
            or not toolchain["msbuild_version"].strip()
            or not invocations_ok
            or toolchain.get("configuration") != "Release"
            or toolchain.get("platform") != "x64"):
        problems.append("clean x64 Release rebuild toolchain/commands are absent")
    artifacts = receipt.get("artifacts") if isinstance(receipt.get("artifacts"), dict) else {}
    for name in ("replus_asi", "replus_capture_addon", "diogenes_controller_asi",
                 "replus_main_ini", "replus_render_ini", "replus_lights_ini",
                 "replus_settings_exe", "igcs_dof_shader"):
        record = artifacts.get(name) if isinstance(artifacts.get(name), dict) else {}
        component = manifest.raw["components"].get(name, {})
        artifact_path = record.get("path")
        if (not isinstance(artifact_path, str) or not Path(artifact_path).is_absolute()
                or not isinstance(record.get("bytes"), int) or record.get("bytes", 0) <= 0
                or record.get("sha256") != component.get("sha256")):
            problems.append(f"{name} artifact record differs from the deployed component")
    transforms = (receipt.get("configuration_transforms")
                  if isinstance(receipt.get("configuration_transforms"), dict) else {})
    render_transform = (transforms.get("replus_render_ini")
                        if isinstance(transforms.get("replus_render_ini"), dict) else {})
    render_settings = (render_transform.get("settings")
                       if isinstance(render_transform.get("settings"), dict) else {})
    output_folder = render_settings.get("RenderOutputFolder")
    expected_render_settings = dict(REPLUS_RENDER_PROFILE)
    if isinstance(output_folder, str) and "\r" not in output_folder and "\n" not in output_folder:
        expected_render_settings["RenderOutputFolder"] = output_folder
    else:
        problems.append("Render.ini transform has an invalid RenderOutputFolder")
    if (render_transform.get("schema") != REPLUS_RENDER_PROFILE_SCHEMA
            or render_transform.get("algorithm") != REPLUS_RENDER_PROFILE_ALGORITHM
            or render_settings != expected_render_settings):
        problems.append("Render.ini transform does not seal the approved capture profile")
    for side_name in ("input", "output"):
        side = (render_transform.get(side_name)
                if isinstance(render_transform.get(side_name), dict) else {})
        if (not isinstance(side.get("bytes"), int) or isinstance(side.get("bytes"), bool)
                or side.get("bytes", 0) <= 0
                or not isinstance(side.get("sha256"), str)
                or re.fullmatch(r"[0-9a-f]{64}", side.get("sha256", "")) is None):
            problems.append(f"Render.ini transform {side_name} hash/length is invalid")
    render_artifact = (artifacts.get("replus_render_ini")
                       if isinstance(artifacts.get("replus_render_ini"), dict) else {})
    render_output = (render_transform.get("output")
                     if isinstance(render_transform.get("output"), dict) else {})
    if (render_output.get("bytes") != render_artifact.get("bytes")
            or render_output.get("sha256") != render_artifact.get("sha256")):
        problems.append("Render.ini transform output differs from the sealed artifact")
    return Check(
        "dependencies.capture_build_receipt", "FAIL" if problems else "PASS",
        "v18 sources, toolchain, Render.ini transform, and deployed artifacts are bound"
        if not problems else "capture build provenance is incomplete or mismatched",
        {"path": str(receipt_path) if receipt_path else None,
         "controller_git_commit": controller_commit, "problems": problems},
    )


def run_preflight(manifest: DependencyManifest, *, mode: str) -> PreflightReport:
    if mode not in {"dry-run", "real"}:
        raise ContractError("preflight mode must be dry-run or real")
    real = mode == "real"
    checks: list[Check] = []

    policy_problems = manifest.policy.validate()
    checks.append(Check("policy.capture_boundary", "FAIL" if policy_problems else "PASS",
                        "; ".join(policy_problems) if policy_problems else
                        "capture policy is internally consistent",
                        manifest.policy.to_dict()))

    repository_blocked = repository_profile_non_trainable()
    repo_status = "FAIL" if (manifest.policy.purpose == "commercial_delivery"
                             and repository_blocked) else "PASS"
    checks.append(Check(
        "policy.repository_profile",
        repo_status,
        "checked-in GTA profile remains non-trainable" if repository_blocked else
        "checked-in GTA profile permits trainable capture",
        {"path": str(repository_profile_path()), "non_trainable": repository_blocked},
    ))

    is_windows = platform.system() == "Windows"
    checks.append(Check("platform.windows", "PASS" if is_windows else ("FAIL" if real else "WARN"),
                        f"host platform is {platform.system()}"))

    render = manifest.raw["render"]
    expected_render = {
        "fps": 30,
        "graphics_api": "DX11",
        "frame_step_mode": "fixed_replay_timeline",
        "render_samples": 1,
        "motion_blur": False,
        "width": 1920,
        "height": 1080,
    }
    wrong = {key: {"expected": expected, "actual": render.get(key)}
             for key, expected in expected_render.items() if render.get(key) != expected}
    checks.append(Check("render.contract", "FAIL" if wrong else "PASS",
                        "render contract mismatch" if wrong else "DX11 fixed 30fps replay contract selected",
                        wrong or render))

    source_pins = manifest.raw.get("source_pins", {})
    # A v3-looking JSON or a new hash alone never selects the new source pin.
    # Reuse this complete result below; the original v1/v2 constants stay fixed.
    capture_build_check = (_check_capture_build_receipt(manifest)
                           if real or is_reviewed_replus_patch(source_pins.get("replus_patch_sha256")) else None)
    expected_source_pins = {
        "scripthook_runtime_zip_sha256":
            "b64c97c3353906f14621e7e9511e4aec2a7d436ecc21ed124d3816585e2e6188",
        "scripthook_sdk_zip_sha256":
            "56b6ad265b2b93aa4e73f79f97949d72f3683a35576b977e63dde91c6485fe22",
        "replus_commit": "c6eba57160ca444817b149b3304ed1454e0ba2d9",
        "replus_patch_sha256":
            "4582b99830b47f6816b16ccf16dec274d90d9c822430be3f482c3f610cad03aa",
        "igcs_fork_commit": "13c92ef2565403ffcc2501f779ca7cd935e5d598",
        "igcs_patch_sha256":
            "5acb2d375748a0ee91ee7fc0c5f0f38e349139dfff0e78963c4ace89f524d4f5",
        "reshade_setup_sha256": RESHADE_SETUP_SHA256,
        "reshade_signer_thumbprint": RESHADE_SIGNER,
    }
    if (capture_build_check is not None and capture_build_check.status == "PASS"
            and (capture_build_check.evidence or {}).get("kind") in
                {"replus_only_derived", CONTROLLER_REPLUS_KIND}):
        checked_patch = capture_build_check.evidence.get("reviewed_replus_patch_sha256")
        if is_reviewed_replus_patch(checked_patch):
            expected_source_pins["replus_patch_sha256"] = checked_patch
    bad_pins = {key: {"expected": value, "actual": source_pins.get(key)}
                for key, value in expected_source_pins.items()
                if source_pins.get(key) != value}
    pin_status = "FAIL" if real and bad_pins else ("WARN" if bad_pins else "PASS")
    checks.append(Check("dependencies.source_pins", pin_status,
                        "pinned source/patch contract mismatch" if bad_pins else
                        "source commits, patches, and installers are pinned",
                        bad_pins or expected_source_pins))

    calibration = manifest.raw["calibration"]
    acceptance = calibration.get("acceptance")
    calibration_ok = (
        _is_approved_id(calibration.get("calibration_id"))
        and _is_approved_id(calibration.get("filmback_id"))
        and calibration.get("metric_scale_approved") is True
        and isinstance(calibration.get("metric_scale"), (int, float))
        and not isinstance(calibration.get("metric_scale"), bool)
        and math.isfinite(float(calibration.get("metric_scale", 0)))
        and calibration.get("metric_scale", 0) > 0
        and isinstance(calibration.get("projection_evidence"), dict)
        and isinstance(acceptance, dict)
        and isinstance(acceptance.get("max_reprojection_error_px"), (int, float))
        and not isinstance(acceptance.get("max_reprojection_error_px"), bool)
        and acceptance.get("max_reprojection_error_px", 0) > 0
        and isinstance(acceptance.get("min_sample_count"), int)
        and not isinstance(acceptance.get("min_sample_count"), bool)
        and acceptance.get("min_sample_count", 0) > 0
    )
    calibration_status = "PASS" if calibration_ok else ("FAIL" if real else "WARN")
    checks.append(Check("calibration.binding", calibration_status,
                        "filmback and metric scale are bound" if calibration_ok else
                        "filmback/metric scale are pending; real capture is blocked",
                        calibration))

    if real:
        game = manifest.raw["game"]
        game_path = _component_path(manifest, game)
        checks.append(_check_gpu_driver(manifest))
        checks.extend(_check_file("game.gta5_exe", manifest, game, require_pin=True))
        required = (
            "scripthook_dll", "asi_loader", "replus_asi",
            "replus_capture_addon", "reshade_dxgi", "replus_render_ini",
            "replus_main_ini", "replus_lights_ini", "replus_settings_exe",
            "igcs_dof_shader", "reshade_install_receipt", "capture_build_receipt",
            "diogenes_controller_asi", "gta_settings_xml", "ffmpeg", "ffprobe",
        )
        for name in required:
            checks.extend(_check_file(f"component.{name}", manifest,
                                      manifest.raw["components"].get(name), require_pin=True))
        checks.append(capture_build_check)
        checks.append(_check_reshade_receipt(manifest, game_path))
        checks.extend(_check_file("calibration.projection_evidence", manifest,
                                  calibration.get("projection_evidence"), require_pin=True))
        checks.append(_check_projection_calibration(manifest, calibration))
        build = game.get("expected_build")
        actual_build = _actual_windows_file_version(game_path) if game_path else None
        build_ok = build == "1.0.3889.0" and actual_build == build
        checks.append(Check("game.legacy_build", "PASS" if build_ok else "FAIL",
                            "actual GTA Legacy build matches 1.0.3889.0" if build_ok else
                            "actual GTA5.exe FileVersion does not match the pinned Legacy build",
                            {"expected": build, "actual": actual_build}))
        unexpected_asi = []
        unexpected_addon = []
        if game_path is not None and game_path.parent.is_dir():
            allowed = _approved_game_root_asi_names(manifest, game_path)
            unexpected_asi = sorted(
                str(path.resolve()) for path in game_path.parent.glob("*.asi")
                if path.name.casefold() not in allowed)
            unexpected_addon = sorted(
                str(path.resolve()) for path in game_path.parent.glob("*.addon64")
                if path.name.casefold() != "igcsconnector.addon64")
        checks.append(Check(
            "dependencies.asi_allowlist", "FAIL" if unexpected_asi else "PASS",
            "game root contains only approved capture ASI modules" if not unexpected_asi else
            "unapproved ASI modules can alter capture semantics",
            {"unexpected": unexpected_asi},
        ))
        checks.append(Check(
            "dependencies.addon_allowlist", "FAIL" if unexpected_addon else "PASS",
            "game root contains only the approved capture ReShade addon"
            if not unexpected_addon else "unapproved ReShade addons can alter captured pixels",
            {"unexpected": unexpected_addon},
        ))
        checks.extend(_check_render_files(manifest))
    else:
        gpu = manifest.raw["gpu"]
        gpu_pinned = (gpu.get("vendor") == "NVIDIA"
                      and isinstance(gpu.get("adapter_name"), str)
                      and bool(gpu.get("adapter_name").strip())
                      and "pending" not in gpu.get("adapter_name").casefold()
                      and isinstance(gpu.get("driver_version"), str)
                      and bool(gpu.get("driver_version").strip())
                      and "pending" not in gpu.get("driver_version").casefold())
        checks.append(Check("environment.gpu_driver_lock", "PASS" if gpu_pinned else "WARN",
                            "GPU driver lock is present" if gpu_pinned else
                            "GPU driver lock is pending; real capture will be blocked", gpu))
        checks.append(Check("dependencies.real_capture", "SKIP",
                            "game and ASI binaries are not required for dry-run"))

    return PreflightReport(mode, checks)
