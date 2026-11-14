"""Teacher annotation: bench images -> structured observation JSON (SFT pairs).

Backends (doc 16 §4.4 — teacher must be the open Qwen3-VL big model; closed
frontier models are a legal red line, not a fallback):

  stub    deterministic labels derived from ground truth in the manifest
          (game field + held keys). No model, no network — CI / pipeline
          validation, and a floor check that SFT learns *something*.
  api     any OpenAI-compatible chat endpoint serving Qwen3-VL-235B
          (DashScope, vLLM, sglang...). Set --base-url/--model,
          api key via OPENAI_API_KEY or --api-key-env.
  local   a transformers checkpoint on this machine (for small-scale tests).
  gemini  Vertex AI Gemini — **EVALUATION-ONLY** (doc 16 §4.4). Google's terms
          forbid using outputs to create or improve competing models, so these
          labels may benchmark the open teachers' label ceiling but must never
          become training data. Rows are written with eval_only=true and the
          SFT loader refuses them.

Output: labels.jsonl rows {**manifest_row, "observation": {...}, "teacher": name}.
Rows whose responses fail schema validation are dropped (count reported).

    python -m diogenes_vl.teacher --bench data/bench_synth --backend stub \
        --out data/bench_synth/labels.jsonl
"""
from __future__ import annotations

import argparse
import json
import os
import threading
from pathlib import Path

from .bench import load_manifest
from .schema import PROMPT, SchemaError, parse_observation, validate_observation

# backends that talk over the network and therefore benefit from threading
NETWORK_BACKENDS = frozenset({"api", "gemini"})


# ---------------------------------------------------------------------------
# backends
# ---------------------------------------------------------------------------

def stub_observation(row: dict) -> dict:
    keys = set(row.get("keys", []))
    if 1 in keys:
        event_class, event, score = "combat_kill", "player fires at a target", 0.9
    elif keys & {87, 65, 83, 68}:
        event_class, event, score = "movement", "player moves through the scene", 0.2
    else:
        event_class, event, score = "idle", "nothing notable happens", 0.05
    return validate_observation({
        "game": row["game"],
        "scene": f"a scene from {row['game']}",
        "event": event,
        "event_class": event_class,
        "subjects": ["player"],
        "ocr_text": [],
        "highlight_score": score,
    })


class ApiTeacher:
    """OpenAI-compatible client with retries.

    A serving hiccup (500 from an overloaded engine, a dropped connection)
    must not permanently drop a label — an unretried run lost 2841/3040
    labels to transient 500s. Retries use exponential backoff with a long
    final wait so an engine restart can be ridden out.
    """

    def __init__(self, base_url: str, model: str, api_key_env: str = "OPENAI_API_KEY",
                 max_attempts: int = 5, timeout: int = 180):
        import urllib.request
        self._urllib = urllib.request
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.api_key = os.environ.get(api_key_env, "")
        self.max_attempts = max_attempts
        self.timeout = timeout

    def __call__(self, bench_dir: Path, row: dict) -> dict:
        import base64
        import time
        img = (bench_dir / row["image"]).read_bytes()
        payload = {
            "model": self.model,
            "temperature": 0.0,
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {
                    "url": "data:image/png;base64," + base64.b64encode(img).decode()}},
                {"type": "text", "text": PROMPT},
            ]}],
        }
        last: Exception | None = None
        for attempt in range(self.max_attempts):
            req = self._urllib.Request(
                f"{self.base_url}/chat/completions",
                data=json.dumps(payload).encode(),
                headers={"Content-Type": "application/json",
                         "Authorization": f"Bearer {self.api_key}"})
            try:
                with self._urllib.urlopen(req, timeout=self.timeout) as resp:
                    body = json.load(resp)
                return parse_observation(body["choices"][0]["message"]["content"])
            except (OSError, ValueError, SchemaError) as e:
                last = e
                if attempt < self.max_attempts - 1:
                    time.sleep(min(60, 2 ** attempt * 3))
        raise last  # type: ignore[misc]


class GeminiTeacher:
    """Vertex AI Gemini — EVALUATION-ONLY teacher (see module docstring).

    Auth comes from gcloud application-default credentials; no key handling.
    Labels it produces carry eval_only=true so they cannot silently become
    training data.
    """

    EVAL_ONLY = True

    def __init__(self, model: str, project: str, location: str = "us-central1"):
        import subprocess
        import urllib.request
        self._urllib = urllib.request
        self.model = model
        self.url = (f"https://{location}-aiplatform.googleapis.com/v1/projects/"
                    f"{project}/locations/{location}/publishers/google/models/"
                    f"{model}:generateContent")
        self._token = subprocess.run(
            ["gcloud", "auth", "application-default", "print-access-token"],
            capture_output=True, text=True, check=True).stdout.strip()

    def __call__(self, bench_dir: Path, row: dict) -> dict:
        import base64
        import time
        img = (bench_dir / row["image"]).read_bytes()
        payload = {
            "contents": [{"role": "user", "parts": [
                {"inline_data": {"mime_type": "image/png",
                                 "data": base64.b64encode(img).decode()}},
                {"text": PROMPT},
            ]}],
            "generationConfig": {
                "temperature": 0.0,
                "maxOutputTokens": 1024,
                "responseMimeType": "application/json",
                # 2.5 models think by default and thinking tokens eat the
                # output budget; the task is perception, not reasoning
                "thinkingConfig": {"thinkingBudget": 0},
            },
        }
        last: Exception | None = None
        for attempt in range(5):
            req = self._urllib.Request(
                self.url, data=json.dumps(payload).encode(),
                headers={"Content-Type": "application/json",
                         "Authorization": f"Bearer {self._token}"})
            try:
                with self._urllib.urlopen(req, timeout=120) as resp:
                    body = json.load(resp)
                text = body["candidates"][0]["content"]["parts"][0]["text"]
                return parse_observation(text)
            except (OSError, ValueError, KeyError, IndexError, SchemaError) as e:
                last = e
                if attempt < 4:
                    time.sleep(min(60, 2 ** attempt * 3))
        raise last  # type: ignore[misc]


class LocalTeacher:
    def __init__(self, model_name: str, device: str = "auto"):
        import torch
        from transformers import AutoModelForImageTextToText, AutoProcessor
        self.processor = AutoProcessor.from_pretrained(model_name)
        self.model = AutoModelForImageTextToText.from_pretrained(
            model_name, dtype=torch.bfloat16 if torch.cuda.is_available() else torch.float32,
            device_map=device)
        self.model.eval()

    def __call__(self, bench_dir: Path, row: dict) -> dict:
        import torch
        from PIL import Image
        img = Image.open(bench_dir / row["image"]).convert("RGB")
        messages = [{"role": "user", "content": [
            {"type": "image", "image": img}, {"type": "text", "text": PROMPT}]}]
        inputs = self.processor.apply_chat_template(
            messages, add_generation_prompt=True, tokenize=True,
            return_dict=True, return_tensors="pt").to(self.model.device)
        with torch.no_grad():
            out = self.model.generate(**inputs, max_new_tokens=256, do_sample=False)
        text = self.processor.decode(out[0][inputs["input_ids"].shape[1]:],
                                     skip_special_tokens=True)
        return parse_observation(text)


# ---------------------------------------------------------------------------

def annotate(bench_dir: str | Path, backend: str, out_path: str | Path, *,
             base_url: str | None = None, model: str | None = None,
             api_key_env: str = "OPENAI_API_KEY", limit: int | None = None,
             concurrency: int = 1, project: str | None = None,
             location: str = "us-central1", resume: bool = True) -> dict:
    bench_dir = Path(bench_dir)
    rows = load_manifest(bench_dir)
    if limit:
        rows = rows[:limit]

    teacher = None
    if backend == "api":
        assert base_url and model, "--base-url and --model required for api backend"
        teacher = ApiTeacher(base_url, model, api_key_env)
    elif backend == "local":
        assert model, "--model required for local backend"
        teacher = LocalTeacher(model)
    elif backend == "gemini":
        assert model and project, "--model and --project required for gemini"
        teacher = GeminiTeacher(model, project, location)
    elif backend != "stub":
        raise SystemExit(f"unknown backend {backend}")
    eval_only = bool(getattr(teacher, "EVAL_ONLY", False))

    teacher_tag = backend if backend == "stub" else f"{backend}:{model}"
    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    # Resume: a preempted spot VM or a killed run leaves a partial file;
    # re-annotating hours of frames to recover is pure waste.
    done: set[str] = set()
    if resume and out_path.exists():
        with open(out_path) as f:
            for line in f:
                if line.strip():
                    done.add(json.loads(line)["id"])
        rows = [r for r in rows if r["id"] not in done]
        print(f"resuming: {len(done)} already labeled, {len(rows)} to go")

    def label_one(row: dict):
        try:
            obs = stub_observation(row) if backend == "stub" else teacher(bench_dir, row)
            return row, obs, None
        except (SchemaError, OSError) as e:
            return row, None, e

    n_ok, n_bad = len(done), 0
    write_lock = threading.Lock()
    # append incrementally so a preemption costs only in-flight work
    with open(out_path, "a" if done else "w") as f:
        def handle(result):
            nonlocal n_ok, n_bad
            row, obs, err = result
            if obs is None:
                with write_lock:
                    n_bad += 1
                print(f"  drop {row['id']}: {err}")
                return
            rec = {**row, "observation": obs, "teacher": teacher_tag}
            if eval_only:
                rec["eval_only"] = True  # SFT loader refuses these
            with write_lock:
                f.write(json.dumps(rec) + "\n")
                f.flush()
                n_ok += 1
                if n_ok % 200 == 0:
                    print(f"  ... {n_ok} labeled")

        # network-backed teachers parallelize; `local` runs on this GPU and
        # `stub` is pure CPU, so both stay serial.
        # as_completed, not map: map yields in submission order, so one slow
        # row (a retry storm) holds every finished result in memory behind it
        # and nothing reaches disk — which defeats the incremental write.
        if concurrency > 1 and backend in NETWORK_BACKENDS:
            from concurrent.futures import ThreadPoolExecutor, as_completed
            with ThreadPoolExecutor(concurrency) as pool:
                futures = [pool.submit(label_one, r) for r in rows]
                for fut in as_completed(futures):
                    handle(fut.result())
        else:
            for row in rows:
                handle(label_one(row))

    if n_bad > 0.2 * max(1, len(rows)):
        raise SystemExit(
            f"aborting: {n_bad}/{len(rows)} labels failed even after retries "
            "— the serving side is unhealthy (check for engine OOM/restarts). "
            "Fix the server rather than training on a decimated label set.")

    stats = {"labeled": n_ok, "dropped_invalid": n_bad, "backend": backend,
             "eval_only": eval_only, "resumed_from": len(done)}
    print(json.dumps(stats))
    return stats


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--bench", required=True)
    p.add_argument("--backend", choices=["stub", "api", "local", "gemini"],
                   default="stub")
    p.add_argument("--out", required=True)
    p.add_argument("--base-url", default=None,
                   help="OpenAI-compatible endpoint serving the OPEN teacher "
                        "(Qwen3-VL-235B); closed models are training-data red line")
    p.add_argument("--project", default=None, help="GCP project (gemini backend)")
    p.add_argument("--location", default="us-central1", help="Vertex AI region")
    p.add_argument("--model", default=None)
    p.add_argument("--api-key-env", default="OPENAI_API_KEY")
    p.add_argument("--limit", type=int, default=None)
    p.add_argument("--concurrency", type=int, default=1,
                   help="parallel requests (api backend; vLLM batches them)")
    args = p.parse_args()
    annotate(args.bench, args.backend, args.out, base_url=args.base_url,
             model=args.model, api_key_env=args.api_key_env, limit=args.limit,
             concurrency=args.concurrency, project=args.project,
             location=args.location)


if __name__ == "__main__":
    main()
