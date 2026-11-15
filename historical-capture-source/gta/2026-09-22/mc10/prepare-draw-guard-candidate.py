"""Prepare a new frozen RE+ source candidate. No build, deployment, or game access."""
from pathlib import Path
import datetime as dt
import difflib
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent
PARENT = ROOT / "replus-cursor-refresh-candidate-03"
TARGET = ROOT / "replus-draw-guard-candidate-01"

DRAW_GUARD = r'''
		// The draw worker executes on both the frontend and render threads. Only
		// atomic state is read here; plan ownership, logging, code-byte checks and
		// terminal transitions belong to the editor owner, never this callback.
		std::atomic<bool> s_movieOwnedSuppression{false};
		std::atomic<bool> s_movieHideHud{true};
		std::atomic<bool> s_movieCallbackFault{false};
		std::atomic<uint64_t> s_movieAllowed{0}, s_movieSuppressed{0};
		std::atomic<uint64_t> s_movieKeyOverflow{0};
		struct MovieObservation
		{
			std::atomic<uint64_t> key{0}; // thread id in high 32 bits; movie id in low 32
			std::atomic<uint64_t> suppressed{0};
		};
		constexpr unsigned kMovieObservationSlots = 16;
		MovieObservation s_movieObservations[kMovieObservationSlots];
		// Immutable after hook installation; only inspected by the owner.
		bool s_movieHookInstalled = false;
		uint8_t s_movieHookBytes[16]{}, s_movieTrampolineBytes[16]{};
		FnDrawMovie s_movieInstalledTrampoline = nullptr;
		DWORD s_movieOwner = 0;
		bool s_movieEnding = false;
		int s_movieLastLogFrame = -1;
		uint64_t s_movieAllowedAtArm = 0, s_movieSuppressedAtArm = 0;

		constexpr bool movieGuardMayArm(DWORD thread, int frame, bool hookReady,
		                               bool acknowledged, bool ownedEvidence)
		{
			return thread != 0 && frame == 0 && hookReady && acknowledged && ownedEvidence;
		}
		constexpr bool movieGuardMayRelease(DWORD owner, DWORD thread,
		                                   bool ending, bool acknowledged)
		{
			return owner != 0 && owner == thread && ending && acknowledged;
		}
		static_assert(movieGuardMayArm(7, 0, true, true, true));
		static_assert(!movieGuardMayArm(0, 0, true, true, true));
		static_assert(!movieGuardMayArm(7, 1, true, true, true));
		static_assert(!movieGuardMayArm(7, 0, false, true, true));
		static_assert(!movieGuardMayArm(7, 0, true, false, true));
		static_assert(!movieGuardMayArm(7, 0, true, true, false));
		static_assert(movieGuardMayRelease(7, 7, true, true));
		static_assert(!movieGuardMayRelease(7, 8, true, true));
		static_assert(!movieGuardMayRelease(7, 7, false, true));
		static_assert(!movieGuardMayRelease(7, 7, true, false));
		static_assert(std::atomic<Step>::is_always_lock_free);
		static_assert(std::atomic<bool>::is_always_lock_free);
		static_assert(std::atomic<uint64_t>::is_always_lock_free);

		bool movieHookIntact()
		{
			return s_movieHookInstalled && game::addr_ScaleformDrawMovie && origDrawMovie &&
			       origDrawMovie == s_movieInstalledTrampoline &&
			       memcmp(s_movieHookBytes, (const void*)game::addr_ScaleformDrawMovie,
			              sizeof(s_movieHookBytes)) == 0 &&
			       memcmp(s_movieTrampolineBytes, (const void*)origDrawMovie,
			              sizeof(s_movieTrampolineBytes)) == 0;
		}

		void logMovieGuard(const char* phase, bool slots)
		{
			if (!s_movieOwner || GetCurrentThreadId() != s_movieOwner) return;
			// These are bounded process-lifetime callback observations, not a
			// one-to-one mapping from movie calls to PNGs or actual cursor instances.
			logger::write("info",
				"render: movie guard %s frame=%d request=%u owner=%lu active=%d ending=%d "
				"ack=%d hookIntact=%d allowedSinceArm=%llu suppressedSinceArm=%llu keyOverflowLifetime=%llu",
				phase, s_frame, fxcapture::requestId(), s_movieOwner,
				s_movieOwnedSuppression.load(std::memory_order_acquire) ? 1 : 0,
				s_movieEnding ? 1 : 0, fxcapture::lastDone() ? 1 : 0, movieHookIntact() ? 1 : 0,
				s_movieAllowed.load(std::memory_order_relaxed) - s_movieAllowedAtArm,
				s_movieSuppressed.load(std::memory_order_relaxed) - s_movieSuppressedAtArm,
				s_movieKeyOverflow.load(std::memory_order_relaxed));
			if (!slots) return;
			for (unsigned i = 0; i < kMovieObservationSlots; ++i)
			{
				const uint64_t key = s_movieObservations[i].key.load(std::memory_order_acquire);
				if (key) logger::write("info",
					"render: movie guard observed slot=%u movieId=%u thread=%lu suppressedLifetime=%llu",
					i, static_cast<unsigned>(key), static_cast<DWORD>(key >> 32),
					s_movieObservations[i].suppressed.load(std::memory_order_relaxed));
			}
		}

		// Called only by the editor owner, including while the renderer is Idle.
		// finish() still makes the bridge seal FAILED immediately. Its native
		// failure drainer then releases the unchanged in-flight request; we retain
		// suppression until the add-on's actual ACK, without writing protocol state.
		bool serviceMovieGuardDrain()
		{
			if (!s_movieOwnedSuppression.load(std::memory_order_acquire)) return true;
			if (GetCurrentThreadId() != s_movieOwner)
			{
				s_movieCallbackFault.store(true, std::memory_order_release);
				return false;
			}
			if (!s_movieEnding) return true;
			if (!movieGuardMayRelease(s_movieOwner, GetCurrentThreadId(), true, fxcapture::lastDone()))
				return false;
			logMovieGuard("drained", true);
			s_movieOwnedSuppression.store(false, std::memory_order_release);
			s_movieEnding = false;
			s_movieOwner = 0;
			return true;
		}

		void endMovieGuard()
		{
			if (!s_movieOwnedSuppression.load(std::memory_order_acquire)) return;
			if (GetCurrentThreadId() != s_movieOwner)
			{
				s_movieCallbackFault.store(true, std::memory_order_release);
				return;
			}
			s_movieEnding = true;
			logMovieGuard("terminal", false);
			serviceMovieGuardDrain();
		}

		const char* prepareMovieGuard(bool ownedEvidence, bool& newlyArmed)
		{
			newlyArmed = false;
			if (!movieHookIntact()) return "movie draw hook absent or installed bytes changed";
			if (s_movieCallbackFault.load(std::memory_order_acquire)) return "movie callback/owner fault";
			if (!s_movieOwnedSuppression.load(std::memory_order_acquire))
			{
				if (!movieGuardMayArm(GetCurrentThreadId(), s_frame, true,
				                     fxcapture::lastDone(), ownedEvidence))
					return "movie guard requires owned frame zero and an idle request fence";
				s_movieOwner = GetCurrentThreadId();
				s_movieEnding = false;
				s_movieLastLogFrame = -1;
				s_movieAllowedAtArm = s_movieAllowed.load(std::memory_order_relaxed);
				s_movieSuppressedAtArm = s_movieSuppressed.load(std::memory_order_relaxed);
				s_movieOwnedSuppression.store(true, std::memory_order_release);
				newlyArmed = true;
				logMovieGuard("armed", true);
			}
			if (s_movieOwner != GetCurrentThreadId() || s_movieEnding || !ownedEvidence)
				return "movie guard owner or active plan evidence changed";
			if (s_frame != s_movieLastLogFrame &&
			    (s_frame == 0 || (s_frame % 120) == 0 || (s_frame >= 192 && s_frame <= 200)))
			{
				logMovieGuard("before-request", false);
				s_movieLastLogFrame = s_frame;
			}
			return nullptr;
		}
'''

MOVIE_CALLBACK = r'''		void __fastcall hkDrawMovie(unsigned int movieId, void* pos, void* scale,
		                            void* depth, void* colour,
		                            unsigned int a, unsigned int b, char c, char d)
		{
			const bool owned = s_movieOwnedSuppression.load(std::memory_order_acquire);
			// dofsession::active() is already an atomic read in the sealed upstream.
			// The owned latch has priority and is independent of config/mode changes.
			const bool suppress = owned ||
				((s_step.load(std::memory_order_acquire) != Step::Idle || dofsession::active()) &&
				 s_movieHideHud.load(std::memory_order_acquire));
			if (suppress)
			{
				s_movieSuppressed.fetch_add(1, std::memory_order_relaxed);
				if (owned)
				{
					const uint64_t key = (static_cast<uint64_t>(GetCurrentThreadId()) << 32) | movieId;
					bool stored = false;
					for (unsigned i = 0; i < kMovieObservationSlots; ++i)
					{
						uint64_t observed = s_movieObservations[i].key.load(std::memory_order_acquire);
						if (!observed)
							s_movieObservations[i].key.compare_exchange_strong(observed, key,
								std::memory_order_acq_rel, std::memory_order_acquire);
						if (observed == key || (!observed &&
						    s_movieObservations[i].key.load(std::memory_order_acquire) == key))
						{
							s_movieObservations[i].suppressed.fetch_add(1, std::memory_order_relaxed);
							stored = true;
							break;
						}
					}
					if (!stored) s_movieKeyOverflow.fetch_add(1, std::memory_order_relaxed);
				}
				return;
			}
			s_movieAllowed.fetch_add(1, std::memory_order_relaxed);
			if (!origDrawMovie)
			{
				s_movieCallbackFault.store(true, std::memory_order_release);
				return;
			}
			origDrawMovie(movieId, pos, scale, depth, colour, a, b, c, d);
		}
'''

def record(path):
    data = path.read_bytes()
    return {"path": str(path.resolve()), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}

def inventory(root):
    values = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink() or getattr(path, "is_junction", lambda: False)():
            raise ValueError("linked source is not supported")
        if path.is_file():
            row = record(path)
            row["path"] = path.relative_to(root).as_posix()
            values.append(row)
    return values

def replace_once(text, before, after):
    if text.count(before) != 1:
        raise ValueError("source anchor is not unique: " + before[:80])
    return text.replace(before, after, 1)

def dump(path, value):
    with path.open("x", encoding="utf-8", newline="\n") as out:
        json.dump(value, out, ensure_ascii=False, indent=2)
        out.write("\n")

def main():
    prior = json.loads((PARENT / "source-inventory.json").read_text(encoding="utf-8"))
    if inventory(PARENT / "source") != prior["source_files"]:
        raise ValueError("prior frozen source inventory changed")
    TARGET.mkdir(exist_ok=False)
    shutil.copytree(PARENT / "source", TARGET / "source")
    shutil.copyfile(PARENT / "base-source.tar", TARGET / "base-source.tar")
    path = TARGET / "source/src/capture/render.cpp"
    old = path.read_text(encoding="utf-8")
    new = replace_once(old, "Step  s_step   = Step::Idle;", "std::atomic<Step> s_step{Step::Idle};")
    new = replace_once(new, "\t\tFnDrawMovie origDrawMovie = nullptr;", "\t\tFnDrawMovie origDrawMovie = nullptr;\n" + DRAW_GUARD)
    begin = new.index("\t\tvoid __fastcall hkDrawMovie(")
    end = new.index("\n\t\tuint32_t s_lastBeat", begin)
    new = new[:begin] + MOVIE_CALLBACK + new[end:]
    new = replace_once(new,
        '\t\t\tmemory(game::addr_ScaleformDrawMovie).hook(hkDrawMovie, &origDrawMovie, "ScaleformDrawMovie");\n\t\t\tlogger::write("info", "render: scaleform movie rendering suppressed during capture");',
        '''\t\t\tuint8_t before[16]{};
\t\t\tmemcpy(before, (const void*)game::addr_ScaleformDrawMovie, sizeof(before));
\t\t\tconst bool installed = memory(game::addr_ScaleformDrawMovie).hook(
\t\t\t\thkDrawMovie, &origDrawMovie, "ScaleformDrawMovie");
\t\t\tif (installed && origDrawMovie &&
\t\t\t    memcmp(before, (const void*)game::addr_ScaleformDrawMovie, sizeof(before)) != 0)
\t\t\t{
\t\t\t\tmemcpy(s_movieHookBytes, (const void*)game::addr_ScaleformDrawMovie, sizeof(s_movieHookBytes));
\t\t\t\tmemcpy(s_movieTrampolineBytes, (const void*)origDrawMovie, sizeof(s_movieTrampolineBytes));
\t\t\t\ts_movieInstalledTrampoline = origDrawMovie;
\t\t\t\ts_movieHookInstalled = true;
\t\t\t}
\t\t\tlogger::write(s_movieHookInstalled ? "info" : "error",
\t\t\t\t"render: movie draw hook installResult=%d prologueChanged=%d target=%p trampoline=%p checkedBytes=16",
\t\t\t\tinstalled ? 1 : 0, s_movieHookInstalled ? 1 : 0,
\t\t\t\t(const void*)game::addr_ScaleformDrawMovie, (const void*)origDrawMovie);''')
    new = replace_once(new, '\t\tconst Config& c = Config::get();\n',
        '\t\tconst Config& c = Config::get();\n\t\ts_movieHideHud.store(c.renderHideHud, std::memory_order_release);\n')
    new = replace_once(new, '\tvoid pump()\n\t{\n', '''\tvoid pump()
\t{
\t\t// Owner-only service also runs while Idle, so a failed request can drain
\t\t// without publishing another request or prematurely restoring UI draws.
\t\ts_movieHideHud.store(Config::get().renderHideHud, std::memory_order_release);
\t\tif (!serviceMovieGuardDrain()) return;
''')
    new = replace_once(new, '\t\tif (s_step != Step::Idle) { if (reason) *reason = kBusy; return false; }',
        '\t\tif (s_step != Step::Idle || s_movieOwnedSuppression.load(std::memory_order_acquire))\n\t\t{ if (reason) *reason = kBusy; return false; }')
    # Terminal frame-pass only; the audio-to-rewind early returns stay untouched.
    new = replace_once(new, '\t\t\ts_pointerRefreshCalls = 0;\n\t\t\ts_pointerRefreshLoggedFrame = -1;',
        '\t\t\ts_pointerRefreshCalls = 0;\n\t\t\ts_pointerRefreshLoggedFrame = -1;\n\t\t\tendMovieGuard();')
    new = replace_once(new, '\t\t\t\tif (required)\n\t\t\t\t{\n\t\t\t\t\tif (const char* failure = refreshPointerBetweenSamples())', '''\t\t\t\tif (required)
\t\t\t\t{
\t\t\t\t\tbool newlyArmed = false;
\t\t\t\t\tif (const char* failure = prepareMovieGuard(true, newlyArmed))
\t\t\t\t\t{
\t\t\t\t\t\tlogger::write("error", "render: movie guard refused frame=%d reason=%s", s_frame, failure);
\t\t\t\t\t\tfinish("aborted - movie draw guard failed before sample", false);
\t\t\t\t\t\treturn;
\t\t\t\t\t}
\t\t\t\t\tif (newlyArmed)
\t\t\t\t\t{
\t\t\t\t\t\t// Give the newly published suppression the existing three-tick
\t\t\t\t\t\t// settling interval before publishing the first real request.
\t\t\t\t\t\ts_wait = 3;
\t\t\t\t\t\treturn;
\t\t\t\t\t}
\t\t\t\t\tif (const char* failure = refreshPointerBetweenSamples())''')
    # Do not allow a later settings/plan change to bypass the per-request guard.
    new = replace_once(new, '\n\t\t\tchar path[MAX_PATH];\n\t\t\tbuildPath(path, sizeof(path));\n\t\t\tif (!fxcapture::requestSample(path, s_cfg.samples, s_sample,', '''
\t\t\tif (s_movieOwnedSuppression.load(std::memory_order_acquire) &&
\t\t\t    (!movieHookIntact() || s_movieCallbackFault.load(std::memory_order_acquire) ||
\t\t\t     s_movieOwner != GetCurrentThreadId() || s_movieEnding ||
\t\t\t     !Config::get().renderHideHud || s_cfg.captureMode != 0 || s_cfg.samples != 1 ||
\t\t\t     !diogenesbridge::ownedCaptureEvidenceReady(static_cast<uint32_t>(s_frame))))
\t\t\t{
\t\t\t\tfinish("aborted - active movie draw guard lost before sample", false);
\t\t\t\treturn;
\t\t\t}

\t\t\tchar path[MAX_PATH];
\t\t\tbuildPath(path, sizeof(path));
\t\t\tif (!fxcapture::requestSample(path, s_cfg.samples, s_sample,''')
    path.write_text(new, encoding="utf-8", newline="\n")
    # Independent, review-friendly delta from the actual preceding frozen source.
    (TARGET / "draw-guard-only.patch").write_text("".join(difflib.unified_diff(
        old.splitlines(True), new.splitlines(True), fromfile="a/src/capture/render.cpp",
        tofile="b/src/capture/render.cpp")), encoding="utf-8", newline="\n")
    frozen = dict(prior, source_snapshot=str((TARGET / "source").resolve()),
                  source_files=inventory(TARGET / "source"))
    if len(frozen["source_files"]) != 79:
        raise ValueError("source closure changed")
    changed = [b["path"] for a, b in zip(prior["source_files"], frozen["source_files"]) if a != b]
    if changed != ["src/capture/render.cpp"]:
        raise ValueError("unexpected source delta: " + str(changed))
    dump(TARGET / "source-inventory.json", frozen)
    # Rebuild one complete patch against the locked base archive. No chained
    # patch sections, Git commit, checkout, or change to the preserved source.
    baseline = {}
    with tarfile.open(TARGET / "base-source.tar") as archive:
        for member in archive.getmembers():
            if member.isdir():
                continue
            if not member.isfile() or Path(member.name).is_absolute() or ".." in Path(member.name).parts:
                raise ValueError("unsafe source archive member")
            baseline[member.name] = archive.extractfile(member).read()
    current = {row["path"]: (TARGET / "source" / row["path"]).read_bytes() for row in frozen["source_files"]}
    parts = []
    for name in sorted(baseline.keys() | current.keys()):
        a = baseline.get(name, b"").replace(b"\r\n", b"\n")
        b = current.get(name, b"").replace(b"\r\n", b"\n")
        if a == b:
            continue
        parts.append(f"diff --git a/{name} b/{name}\n")
        if name not in baseline:
            parts.append("new file mode 100644\n")
        if name not in current:
            parts.append("deleted file mode 100644\n")
        parts.extend(difflib.unified_diff(a.decode("utf-8").splitlines(True), b.decode("utf-8").splitlines(True),
            fromfile=f"a/{name}" if name in baseline else "/dev/null",
            tofile=f"b/{name}" if name in current else "/dev/null"))
    (TARGET / "source.patch").write_text("".join(parts), encoding="utf-8", newline="\n")
    with tempfile.TemporaryDirectory(prefix="replus-draw-guard-verify-") as directory:
        scratch = Path(directory)
        for name, data in baseline.items():
            dest = scratch / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(data)
        environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        environment["GIT_CEILING_DIRECTORIES"] = str(scratch)
        subprocess.run(["git", "apply", "--binary", "--whitespace=nowarn", str(TARGET / "source.patch")],
                       cwd=scratch, env=environment, check=True, capture_output=True)
        derived = {row["path"]: (scratch / row["path"]).read_bytes() for row in inventory(scratch)}
        if derived.keys() != current.keys() or any(derived[k].replace(b"\r\n", b"\n") != current[k].replace(b"\r\n", b"\n") for k in current):
            raise ValueError("base plus full patch differs from compiled snapshot")
    builder = (PARENT / "build-replus-candidate.py").read_text(encoding="utf-8")
    builder = builder.replace('"contract_assertions": 14', '"contract_assertions": 27')
    builder = builder.replace('Nine compile-time mode assertions and five pointer-thread/recursion assertions; no CTest suite exists in this upstream project.',
        'Nine mode, five pointer-thread, and thirteen movie-guard/lock-free compile-time assertions; no CTest suite exists in this upstream project.')
    builder = builder.replace('Cursor suppression, thread compatibility and frame exchange completion still require real GTA export testing.',
        'Draw suppression, hook identity, callback counts and failure drain still require actual GTA export testing; 16-byte hook/trampoline checks are not whole-module integrity.')
    (TARGET / "build-replus-candidate.py").write_text(builder, encoding="utf-8", newline="\n")
    dump(TARGET / "source-preparation-receipt.json", {
        "schema": "dm-gta-draw-guard-source-preparation/1", "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "preparer": record(Path(__file__)), "parent_source_inventory": record(PARENT / "source-inventory.json"),
        "full_base_archive": record(TARGET / "base-source.tar"), "full_patch": record(TARGET / "source.patch"),
        "delta_patch": record(TARGET / "draw-guard-only.patch"), "inventory": record(TARGET / "source-inventory.json"),
        "builder": record(TARGET / "build-replus-candidate.py"), "changed_files_from_prior_sealed_source": changed,
        "source_file_count": 79, "base_plus_patch_reconstruction_verified": True,
        "contract_assertions": 27, "asi_built": False, "deployed": False, "runtime_tested": False,
        "source_pixels_modified": False})
    if inventory(PARENT / "source") != prior["source_files"]:
        raise ValueError("preserved source changed during preparation")
    print(json.dumps({"candidate": str(TARGET), "patch": record(TARGET / "source.patch"),
                      "builder": record(TARGET / "build-replus-candidate.py"), "source_count": 79}))

if __name__ == "__main__":
    main()
