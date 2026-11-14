#!/usr/bin/env python3
"""catalog 的两条字节级不变量，都在 CI 里查。

**一、跨 bundle：同一个 <kind>:<id>@<version> 不得有两份不同字节。**

版本号是内容的标识——这是整条 catalog 链路的地基假设：客户端按 exact bytes 校验
content_hash，服务端按 (kind, catalog_id, version) 做主键并拒绝"同版本不同字节"
的重复登记，会话把 mapping_version 冻进每条记录用来解释数据语义。

**但在此之前，没有任何东西检查跨 bundle 的这个不变量。** catalog-validator 只在
单个 bundle 内部校验交叉引用与哈希；两个 bundle 各自自洽、却对同一个 id@version
给出不同字节，一路绿灯直到生产边界才被 register:catalog 的守卫逮住。

2026-07-29 实际发生过一次：`config/catalogs/dev/cod-steam-release-test/mapping.json`
与 `config/catalogs/release/cod-steam/mapping.json` 都声称是
`game.call-of-duty.steam.pc@2026-07-24.2`，语义一字不差、排版不同，因而哈希不同。
两条独立血脉（#58 与 #194）从未同步过。生产 register 因此失败——那时才发现。

这个脚本把那道检查提前到 CI。

**二、bundle 内：`catalog.json` 每条 entry 的 `content_hash` 必须等于同目录 payload
文件的实际 sha256。**

这是客户端下发校验与服务端 register 的共同前提，此前在 CI 里同样没人查——
`catalog.json` 手改了 hash 而没改文件（或反过来）能一路绿到生产边界。同时查
manifest 声明了某个 kind 却没有对应 payload 文件的情况：原先那种 bundle 会被
静默跳过，看起来像"没这个 kind"。

⚠️ **这条查不出 2026-08-12 的 FO4 事故。** 那次是 `depth-profile.json` 带着
#739 *之前*的投影参数进 catalog，`content_hash` 与它自洽——旧内容配旧哈希，
字节校验永远绿。真正的修法是去重：同一份事实在仓库里只留一处（130 号 §2.4）。
哈希校验挡的是另一类——两边不同步；它挡不住两边一致地错。
"""
from __future__ import annotations

import collections
import hashlib
import json
import pathlib
import sys

# manifest 里的类别 -> bundle 目录中承载它的文件名
KIND_FILES = {
    "device_profiles": "device-profile.json",
    "capture_profiles": "capture-profile.json",
    "recording_profiles": "recording-profile.json",
    "game_profiles": "game-profile.json",
    "settings_profiles": "settings-profile.json",
    "mappings": "mapping.json",
    "depth_profiles": "depth-profile.json",
    "camera_bindings": "camera-binding.json",
}


def main() -> int:
    root = pathlib.Path(__file__).resolve().parent.parent / "config" / "catalogs"
    if not root.is_dir():
        print(f"catalog root not found: {root}", file=sys.stderr)
        return 2

    hashes: dict[str, set[str]] = collections.defaultdict(set)
    sources: dict[str, list[str]] = collections.defaultdict(list)
    content_hash_failures: list[str] = []
    entries_checked = 0

    for manifest_path in sorted(root.rglob("catalog.json")):
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        for kind, filename in KIND_FILES.items():
            entries = manifest.get(kind, [])
            payload_path = manifest_path.parent / filename
            rel_manifest = manifest_path.relative_to(root.parent.parent)
            if not payload_path.exists():
                # 声明了却没有文件，此前会被静默跳过——看起来像"没这个 kind"。
                for entry in entries:
                    content_hash_failures.append(
                        f"✘ {rel_manifest} declares {kind}[{entry['id']}@{entry['version']}] "
                        f"but {filename} is missing from the bundle"
                    )
                continue
            payload_bytes = payload_path.read_bytes()
            digest = "sha256:" + hashlib.sha256(payload_bytes).hexdigest()
            rel_payload = payload_path.relative_to(root.parent.parent)
            for entry in entries:
                key = f'{kind[:-1]}:{entry["id"]}@{entry["version"]}'
                hashes[key].add(digest)
                sources[key].append(f"{rel_payload} -> {digest}")

                entries_checked += 1
                declared = entry.get("content_hash")
                if declared != digest:
                    content_hash_failures.append(
                        f"✘ {rel_payload} does not match the content_hash {rel_manifest} "
                        f"declares for {key}:\n"
                        f"    manifest says {declared}\n"
                        f"    file bytes are {digest} ({len(payload_bytes)} bytes)"
                    )

    collisions = {key: digests for key, digests in hashes.items() if len(digests) > 1}
    for key, digests in sorted(collisions.items()):
        print(f"✘ {key} is claimed by {len(digests)} different byte sequences:")
        for source in sources[key]:
            print(f"    {source}")
        print("  A version identifies content. Bump one side's version, or make the bytes match.")

    for failure in content_hash_failures:
        print(failure)
    if content_hash_failures:
        print(
            "  content_hash is what the client and register:catalog verify against. "
            "Recompute it from the file, or restore the bytes it was computed from."
        )

    print(
        f"checked {len(hashes)} <kind>:<id>@<version> combinations across "
        f"{len(list(root.rglob('catalog.json')))} bundles — {len(collisions)} collision(s); "
        f"verified {entries_checked} declared content_hash value(s) against file bytes — "
        f"{len(content_hash_failures)} mismatch(es)"
    )
    return 1 if collisions or content_hash_failures else 0


if __name__ == "__main__":
    sys.exit(main())
