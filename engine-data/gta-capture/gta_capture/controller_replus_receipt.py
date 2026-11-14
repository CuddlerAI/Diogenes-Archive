"""A controller-only rebuild on a fully verified, preserved RE+-v3 stack.

The finite chain is v4 -> v3 -> v2 -> v1. No ancestor is rewritten and no
recursive v4 or relaxed v2 parent is accepted. These local records grant no
runtime, calibration, rights, training or delivery approval.
"""

from __future__ import annotations

import copy

from .controller_receipt import _record, _require, _verify_build, _verify_source
from .jsonio import load_json, sha256_file
from .replus_receipt import REPLUS_DERIVED_SCHEMA, validate_replus_receipt


CONTROLLER_REPLUS_DERIVED_SCHEMA = "dm-gta-capture-build-receipt/4"
CONTROLLER_REPLUS_KIND = "controller_on_replus_derived"
CONTROLLER_REPLUS_SCOPE = {
    "only_component_rebuilt": "diogenes_controller_asi",
    "configuration_replaced": False,
    "runtime_verified": False,
    "calibration_approved": False,
    "delivery_eligible": False,
}
_CONTROLLER = "diogenes_controller_asi"
_RECEIPT = "capture_build_receipt"


def validate_preserved_replus_parent(original, receipt_path, controller_backup, *, validate_parent):
    """Resolve hash-checked historical paths without changing any raw manifest."""
    from .preflight import DependencyManifest

    backup = _record(controller_backup, "parent controller backup")
    previous = load_json(receipt_path)
    _require(isinstance(previous, dict) and previous.get("schema") == REPLUS_DERIVED_SCHEMA
             and previous.get("kind") == "replus_only_derived",
             "v4 requires exactly one immutable RE+-v3 parent; recursive derivation is forbidden")
    components = original.raw["components"]
    _require(sha256_file(receipt_path) == components[_RECEIPT].get("sha256")
             and controller_backup["sha256"] == components[_CONTROLLER].get("sha256"),
             "parent receipt/controller backup differs from immutable v3 manifest")

    class PreservedController(DependencyManifest):
        def component_path(self, name):
            if name == _CONTROLLER:
                return backup
            if name == _RECEIPT:
                return receipt_path
            return super().component_path(name)

    view = PreservedController(original.path, original.raw, original.policy)
    effective = validate_replus_receipt(view, previous, validate_parent=validate_parent,
                                       controller_backup=controller_backup)
    artifact = effective["artifacts"][_CONTROLLER]
    _require(all(controller_backup[key] == artifact[key] for key in ("bytes", "sha256")),
             "parent controller backup differs from its actual v2 build artifact")
    return effective


def validate_controller_replus_receipt(manifest, receipt, *, validate_parent):
    """Verify only controller deltas, then return the inherited v1 render profile."""
    from .preflight import DependencyManifest

    _require(isinstance(receipt, dict) and receipt.get("schema") == CONTROLLER_REPLUS_DERIVED_SCHEMA
             and receipt.get("kind") == CONTROLLER_REPLUS_KIND, "v4 receipt kind/schema is unsupported")
    _require(receipt.get("scope") == CONTROLLER_REPLUS_SCOPE,
             "v4 scope must not claim runtime/calibration/delivery approval")
    current = manifest.component_path(_RECEIPT)
    _require(current is not None and current.is_file()
             and sha256_file(current) == manifest.raw["components"][_RECEIPT].get("sha256")
             and load_json(current) == receipt, "v4 receipt differs from current manifest pin")
    parent = receipt.get("parent")
    _require(isinstance(parent, dict), "v4 immutable parent records are absent")
    parent_path = _record(parent.get("manifest"), "parent v3 manifest")
    parent_receipt_path = _record(parent.get("receipt"), "parent v3 receipt")
    original = DependencyManifest.load(parent_path)
    effective = validate_preserved_replus_parent(original, parent_receipt_path,
        parent.get("controller_backup"), validate_parent=validate_parent)

    build_path = _record(receipt.get("controller_build"), "actual controller build receipt")
    build = load_json(build_path)
    _require(isinstance(build, dict) and build.get("schema") == "dm-gta-controller-staging-build/1"
             and build.get("working_tree_candidate") is True and build.get("deployed") is False,
             "v4 requires an actual undeployed controller snapshot build")
    _record(build.get("parent_capture_build_receipt"), "controller build's actual v3 parent")
    _require(all(build["parent_capture_build_receipt"][key] == parent["receipt"][key]
                 for key in ("bytes", "sha256")), "controller build has a different v3 parent")
    source, inventory, digest = _verify_source(build, receipt.get("source_provenance"))
    pins = copy.deepcopy(original.raw["source_pins"])
    pins["controller_git_commit"] = build["git_base_commit"]
    pins["controller_source_inventory_sha256"] = digest
    _require(manifest.raw.get("source_pins") == pins,
             "v4 source pins changed outside controller base/snapshot identity")
    for key in set(original.raw) | set(manifest.raw):
        if key not in {"components", "source_pins"}:
            _require(original.raw.get(key) == manifest.raw.get(key),
                     "non-controller v4 manifest field changed: " + key)
    old_components = original.raw["components"]
    components = manifest.raw["components"]
    _require(set(components) == set(old_components), "v4 component inventory changed")
    for name, item in old_components.items():
        if name in {_CONTROLLER, _RECEIPT}:
            continue
        _require(components[name] == item, "unchanged v4 component pin changed: " + name)
        path = manifest.component_path(name)
        _require(path is not None and path.is_file() and sha256_file(path) == item.get("sha256"),
                 "inherited v4 component bytes changed: " + name)
    artifact = _verify_build(build, receipt.get("build_evidence"), source, inventory, original)
    installed = manifest.component_path(_CONTROLLER)
    _require(installed is not None and installed.is_file()
             and components[_CONTROLLER].get("sha256") == artifact["sha256"]
             and installed.stat().st_size == artifact["bytes"]
             and sha256_file(installed) == artifact["sha256"],
             "v4 controller differs from the actual built ASI")
    result = copy.deepcopy(effective)
    result["artifacts"][_CONTROLLER] = artifact
    return result
