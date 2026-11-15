"""Read-only post-execution audit; writes only a new audit JSON and summary.

This does not delete files or query R2. Preserved upload receipts refer to the
original verification time; discarded full-resolution pixels were not backed up.
"""
from __future__ import annotations

import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

BASE = Path(__file__).resolve().parent
PLAN = BASE / "review/completed-raw-retention-20260910-01"
OUT = BASE / "review/airport-raw-discard-20260920-01"
RAW = Path("C:/capture/gta/replus-output")
RENDERS = {"render_0054": 83, "render_0055": 83, "render_0056": 83, "render_0057": 95}


def read(path):
    return json.loads(Path(path).read_text("utf-8-sig"))


def rows(path):
    return [json.loads(s) for s in Path(path).read_text("utf-8-sig").splitlines() if s.strip()]


def identity(path):
    s = Path(path).stat()
    return {"device": str(s.st_dev), "file_id": str(s.st_ino), "nlink": str(s.st_nlink),
            "bytes": str(s.st_size), "ctime_ns": str(s.st_ctime_ns), "mtime_ns": str(s.st_mtime_ns)}


def normalized(value):
    return {k: str(v) for k, v in value.items()}


def record(path):
    path = Path(path)
    before = identity(path)
    with path.open("rb") as f:
        digest = hashlib.file_digest(f, "sha256").hexdigest()
    assert before == identity(path), f"Changed while hashing: {path}"
    return {"path": str(path), "bytes": int(before["bytes"]), "sha256": digest, "identity": before}


def main():
    target = OUT / "post-execution-audit.json"
    summary = OUT / "SUMMARY-已执行有损清理.md"
    assert not target.exists() and not summary.exists(), "Refuse existing audit output"
    r = read(OUT / "discard-receipt.json")
    assert r["status"] == "completed_intentional_lossy_discard" and r["error"] is None
    assert r["deleted_files"] == 6856 and r["deleted_logical_bytes"] == 24671764858
    assert r["pending_intent"] is None and r["render_whitelist"] == ["0054", "0055", "0056", "0057"]
    for key in ("all_sources_prevalidated", "retained_and_sidecar_final_verification", "original_events_unchanged",
                "protected_run_stat_snapshot_unchanged", "local_20_mp4_sha_verified",
                "finished_package_local_final_verification", "north_package_local_final_verification",
                "mapped_350_sources_identity_unchanged"):
        assert r[key] is True, key
    assert r["complete_remote_backup_verified"] is False and r["exact_pixel_reconstruction_available"] is False
    assert r["upload_performed"] is False and r["finished_package_verified_again_now"] is False
    inputs = [record(OUT / "discard-receipt.json"), record(OUT / "discard-journal.jsonl")]
    for name, digest in r["plan_pins"].items():
        rec = record(PLAN / name)
        assert rec["sha256"] == digest
        inputs.append(rec)
    keep = [x for x in rows(PLAN / "retained-files.jsonl") if x["render"] in RENDERS]
    discard = [x for x in rows(PLAN / "discard-candidates.jsonl") if x["render"] in RENDERS]
    assert len(keep) == 344 and sum(x["bytes"] for x in keep) == 1249189662
    assert len(discard) == 6856 and sum(x["bytes"] for x in discard) == 24671764858
    by_path = {str(Path(x["original_path"])): x for x in discard}
    assert len(by_path) == 6856
    journal = rows(OUT / "discard-journal.jsonl")
    assert len(journal) == 2 * 6856
    seen = set()
    for intent, done in zip(journal[::2], journal[1::2]):
        assert intent["action"] == "intent" and done["action"] == "deleted"
        assert intent["file"] == done["file"] and intent["complete_remote_backup_verified"] is False
        f = intent["file"]
        q = str(Path(f["source_path"]))
        assert q not in seen and q in by_path
        seen.add(q)
        row = by_path[q]
        assert f["bytes"] == row["bytes"] and f["sha256"] == row["sha256"]
        assert f["frame"] == row["frame"] and f["render"] == row["render"]
        assert normalized(f["identity"]) == normalized(row["current_stat"])
        assert not Path(q).exists(), q
    assert seen == set(by_path)
    kept_records = []
    for row in keep:
        q = Path(row["original_path"])
        assert q.parent == RAW / row["render"] and q.name == f"frame_{row['frame']:06d}.png"
        actual = record(q)
        assert actual["bytes"] == row["bytes"] and actual["sha256"] == row["sha256"]
        assert actual["identity"] == normalized(row["current_stat"])
        kept_records.append(actual)
    sidecars = []
    for row in r["sidecars_pre_discard_sha256"]:
        actual = record(row["original_path"])
        assert actual["bytes"] == row["bytes"] and actual["sha256"] == row["sha256"]
        assert actual["identity"] == normalized(row["current_stat"])
        sidecars.append(actual)
    assert len(sidecars) == 8
    members = []
    for render, count in RENDERS.items():
        expected = {Path(x["original_path"]).name for x in keep if x["render"] == render} | {"audio.wav", "assemble.txt"}
        actual = list((RAW / render).iterdir())
        assert len(actual) == count + 2 and {q.name for q in actual} == expected
        assert all(q.is_file() and not q.is_symlink() and not q.is_junction() for q in actual)
        members.append({"render": render, "remaining_pngs": count, "sidecars": 2})
    plan = read(PLAN / "retention-plan.json")
    events = []
    for desc in plan["renders"]:
        if desc["render"] in RENDERS:
            expected = desc["native_event"]
            actual = record(expected["path"])
            assert actual["bytes"] == expected["bytes"] and actual["sha256"] == expected["sha256"]
            events.append(actual)
    assert len(events) == 4
    tree_before = read(OUT / "protected-tree-before.json")
    tree_after = read(OUT / "protected-tree-after.json")
    assert tree_before == tree_after
    for path, encoded in tree_after.items():
        assert identity(path) == normalized(json.loads(encoded)), path
    before = read(OUT / "protected-mapped-sources-before.json")
    after = read(OUT / "protected-mapped-sources-after.json")
    assert before == after and len(after) == 350
    videos = []
    for path, item in after.items():
        assert identity(path) == normalized(item["identity"]), path
        if path.lower().endswith(".mp4"):
            videos.append({"path": path, "bytes": item["bytes"], "sha256": item["sha256"],
                           "content_verified_by_execution_before_after": True, "current_identity_unchanged": True})
    assert len(videos) == 20
    for name in ("protected-tree-before.json", "protected-tree-after.json", "protected-mapped-sources-before.json", "protected-mapped-sources-after.json"):
        inputs.append(record(OUT / name))
    inputs.append(record(BASE / "discard-completed-airport-raw-20260920.ps1"))
    assert inputs[-1]["sha256"] == r["helper_sha256"]
    report = {"schema": "dm-gta-airport-lossy-discard-post-audit/1", "status": "PASS",
              "created_at_utc": datetime.now(timezone.utc).isoformat(), "input_records": inputs,
              "scope": list(RENDERS), "deleted_png_count": 6856, "deleted_logical_bytes": 24671764858,
              "journal_pairs_verified": 6856, "all_candidate_paths_absent": True,
              "retained_pngs_rehashed": kept_records, "sidecars_rehashed": sidecars, "original_event_files": events,
              "remaining_members": members, "protected_tree_files_stat_verified": len(tree_after),
              "protected_source_count": 350, "protected_videos": videos,
              "protected_content_sha_before_after": "Actual execution freshly checked the frozen mappings and both stage packages before and after deletion; this post-audit checks preserved equality and current file identity.",
              "finished_R2_verification_at_utc": r["finished_package_upload_verified_at_utc"], "new_remote_query": False,
              "complete_remote_raw_backup_verified": False, "exact_discarded_pixels_recoverable": False,
              "free_bytes_before": r["free_bytes_before"], "free_bytes_after": r["free_bytes_after"],
              "volume_free_delta_not_exclusive_to_task": r["free_bytes_delta_not_exclusive_to_this_task"],
              "formal_approval_granted": False, "upload_performed": False}
    with target.open("x", encoding="utf-8") as f:
        json.dump(report, f, ensure_ascii=False, indent=2)
        f.write("\n")
    text = ("# 机场原始帧有损清理已完成\n\n"
            "仅处理 `render_0054..0057`：完整 7,200 张 SHA 与封存事件校验通过后，逐张删除 6,856 张 PNG，逻辑字节共 24,671,764,858（24.67 GB）。\n\n"
            "保留 344 张原始 PNG（1,249,189,662 字节）及 8 个 audio/assemble 旁文件；本次后审重新读取其 SHA 全部一致。6,856 对 intent/deleted 日志闭合，全部指定候选路径已不存在，四份原事件未变。\n\n"
            "旧 215 个来源、新北滩 135 个来源、两份冻结 stage 和 20 个最终 MP4 均完成前后内容校验并保持文件身份；本次后审再次比对实际身份。未触碰北滩 RAW0069..0072、其他失败/当前游戏素材。\n\n"
            f"C 盘执行前可用 {r['free_bytes_before']:,} 字节，执行后 {r['free_bytes_after']:,} 字节；卷可用差值受其他进程影响，不等于本任务独占测量。\n\n"
            "这是用户授权的 `intentional_lossy_discard`。机场成片归档使用 2026-09-10 已真实验证的 R2 finished 证据；本次没有发起远端查询或上传。原始 PNG 没有完整远端备份，已删除像素无法由母片、事件或 SHA 精确恢复。正式 QC/标定/人工批准缺口原样保留。\n\n"
            "详见 `discard-receipt.json`、`discard-journal.jsonl` 和 `post-execution-audit.json`。\n")
    summary.write_text(text, encoding="utf-8")
    print(json.dumps({"status": "PASS", "audit": str(target), "sha256": record(target)["sha256"],
                      "retained_pngs": 344, "deleted_pngs": 6856, "deleted_logical_bytes": 24671764858,
                      "protected_tree_files": len(tree_after), "protected_sources": 350, "videos": 20}, ensure_ascii=False))


if __name__ == "__main__":
    main()
