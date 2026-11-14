"""Explicit boundary between internal GTA POC data and delivery-eligible data."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .errors import ContractError, PolicyError


INTERNAL_POC = "internal_nontrainable_poc"
COMMERCIAL_DELIVERY = "commercial_delivery"
PURPOSES = {INTERNAL_POC, COMMERCIAL_DELIVERY}


def repository_profile_path() -> Path:
    return (Path(__file__).resolve().parents[3] / "DataMiner" / "config" / "catalogs" /
            "dev" / "gta5-legacy-pc" / "game-profile.json")


def repository_profile_non_trainable() -> bool:
    """The local manifest cannot relax the checked-in game catalog policy."""
    path = repository_profile_path()
    if not path.is_file():
        return True  # Missing policy is fail-closed.
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return True
    return value.get("non_trainable") is not False


@dataclass(frozen=True)
class CapturePolicy:
    purpose: str
    story_mode_only: bool
    online_forbidden: bool
    non_trainable: bool
    upload_enabled: bool
    customer_replay_sync_accepted: bool
    customer_metadata_semantics_accepted: bool
    commercial_rights_approved: bool

    @classmethod
    def from_dict(cls, value: Any) -> "CapturePolicy":
        if not isinstance(value, dict):
            raise ContractError("manifest.policy must be an object")
        purpose = value.get("purpose")
        if purpose not in PURPOSES:
            raise ContractError(f"manifest.policy.purpose must be one of {sorted(PURPOSES)}")
        required = (
            "story_mode_only", "online_forbidden", "non_trainable", "upload_enabled",
            "customer_replay_sync_accepted", "customer_metadata_semantics_accepted",
            "commercial_rights_approved",
        )
        missing = [key for key in required if not isinstance(value.get(key), bool)]
        if missing:
            raise ContractError("manifest.policy boolean fields missing/invalid: " + ", ".join(missing))
        return cls(purpose=purpose, **{key: value[key] for key in required})

    @property
    def delivery_eligible(self) -> bool:
        return (
            self.purpose == COMMERCIAL_DELIVERY
            and self.story_mode_only
            and self.online_forbidden
            and not self.non_trainable
            and self.upload_enabled
            and self.customer_replay_sync_accepted
            and self.customer_metadata_semantics_accepted
            and self.commercial_rights_approved
        )

    def validate(self) -> list[str]:
        problems: list[str] = []
        if not self.story_mode_only:
            problems.append("story_mode_only must be true")
        if not self.online_forbidden:
            problems.append("online_forbidden must be true")
        if self.purpose == INTERNAL_POC:
            if not self.non_trainable:
                problems.append("internal POC must remain non_trainable")
            if self.upload_enabled:
                problems.append("internal POC must keep upload_enabled=false")
        if self.purpose == COMMERCIAL_DELIVERY and not self.delivery_eligible:
            problems.append("commercial delivery requires all approvals, trainable profile, and upload opt-in")
        return problems

    def require_delivery(self) -> None:
        if not self.delivery_eligible:
            raise PolicyError(
                "delivery is blocked: GTA profile remains non-trainable or contract/rights approvals are missing"
            )

    def to_dict(self) -> dict:
        return {
            "purpose": self.purpose,
            "story_mode_only": self.story_mode_only,
            "online_forbidden": self.online_forbidden,
            "non_trainable": self.non_trainable,
            "upload_enabled": self.upload_enabled,
            "customer_replay_sync_accepted": self.customer_replay_sync_accepted,
            "customer_metadata_semantics_accepted": self.customer_metadata_semantics_accepted,
            "commercial_rights_approved": self.commercial_rights_approved,
            "delivery_eligible": self.delivery_eligible,
        }
