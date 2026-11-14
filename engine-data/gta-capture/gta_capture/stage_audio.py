"""A take's sound before it goes into the videos: the low chopping rumble taken out.

Every take of beach-demo-002 and -003 carries, once or twice and for 30-40 s at a
time, a low chop like a far-off helicopter (the customer, 2026-09-27): some 16
beats a second on 40-200 Hz, from the pier's side of the stage. It is no aircraft:
no take's replay holds a helicopter or a plane (2026-09-28, five clips searched,
beach-demo-003 AB recorded with every aircraft model kept from spawning and the
chop as loud as ever). The game plays it when the Editor replays the clip, so
nothing on the recording side keeps it out.

So it is taken out of the sound. In the seconds that chop, the band below 300 Hz
is swapped for the same band from quiet stretches of the same take: the low end of
a beach is surf and wind, alike from one minute to the next, and what chopped is
gone. 300-450 Hz crossfades; above 450 Hz the sound stays as recorded, and so does
every second that did not chop.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

from .errors import ContractError
from .jsonio import load_json, sha256_file, write_json_atomic

RATE = 48000
BELOW_HZ, ABOVE_HZ = 300.0, 450.0   # swapped below, recorded above, crossfaded between
CHOP_HZ = (8.0, 35.0)               # beats a second that count as chopping
CHOP_SCORE = 6.0                    # a second chops above this (quiet ones score 2-5)
QUIET_SCORE = 5.0                   # a donor second scores below this
PAD_S = 3                           # seconds kept either side of the chopping ones
FADE_S = 1.5                        # in and out of each swapped stretch
SPLICE_S = 0.5                      # between two quiet stretches making one donor

CANDIDATE = "lane-1-audio-candidate.wav"
CLEAN = "lane-1-audio-clean.wav"
REPORT = "audio-clean.json"


def _ffmpeg(ffmpeg: str | None) -> str:
    tool = ffmpeg or shutil.which("ffmpeg")
    if not tool:
        raise ContractError("ffmpeg must be on PATH")
    return tool


def read_audio(path, *, ffmpeg: str | None = None):
    """The WAV as N x 2 float samples at 48 kHz."""
    import numpy as np

    raw = subprocess.run([_ffmpeg(ffmpeg), "-v", "error", "-i", str(path), "-ac", "2", "-ar", str(RATE),
                          "-f", "f32le", "-"], capture_output=True, check=True).stdout
    return np.frombuffer(raw, "<f4").astype(np.float64).reshape(-1, 2)


def write_audio(samples, path, *, ffmpeg: str | None = None) -> Path:
    import numpy as np

    out = Path(path)
    out.parent.mkdir(parents=True, exist_ok=True)
    pcm = np.clip(samples, -1.0, 1.0).astype("<f4")
    subprocess.run([_ffmpeg(ffmpeg), "-v", "error", "-y", "-f", "f32le", "-ar", str(RATE), "-ac", "2", "-i", "-",
                    "-c:a", "pcm_f32le", str(out)], input=pcm.tobytes(), check=True)
    return out


def chop_scores(samples):
    """Per whole second: how strongly the 40-600 Hz loudness beats at 8-35 Hz, against
    its 35-80 Hz floor."""
    import numpy as np

    mono = samples.mean(axis=1)
    f = np.fft.rfftfreq(RATE, 1 / RATE)
    fe = np.fft.rfftfreq(200, 1 / 200)          # the loudness, 200 values a second
    beat = (fe >= CHOP_HZ[0]) & (fe <= CHOP_HZ[1])
    floor = (fe > 35) & (fe <= 80)
    scores = []
    for s in range(len(mono) // RATE):
        spectrum = np.fft.rfft(mono[s * RATE:(s + 1) * RATE] * np.hanning(RATE))
        spectrum[(f < 40) | (f > 600)] = 0
        loudness = np.abs(np.fft.irfft(spectrum, RATE)).reshape(200, -1).mean(axis=1)
        beats = np.abs(np.fft.rfft((loudness - loudness.mean()) * np.hanning(200)))
        scores.append(float(beats[beat].max() / (np.median(beats[floor]) + 1e-12)))
    return np.array(scores)


def _runs(flags) -> list[tuple[int, int]]:
    runs, start = [], None
    for s, on in enumerate(list(flags) + [False]):
        if on and start is None:
            start = s
        elif not on and start is not None:
            runs.append((start, s))
            start = None
    return runs


def chop_windows(scores) -> list[tuple[int, int]]:
    """The stretches to swap, in whole seconds [from, to): the chopping seconds -- two
    of any three in a row, one stray second is nothing -- and PAD_S either side."""
    import numpy as np

    hot = scores > CHOP_SCORE
    hot &= np.convolve(hot, [1, 1, 1], "same") >= 2
    padded = np.convolve(hot, np.ones(2 * PAD_S + 1), "same") > 0
    return _runs(padded)


def _donor(samples, quiet: list[tuple[int, int]], length: int):
    """`length` samples of the take's quiet stretches one after another, spliced with
    SPLICE_S equal-power crossfades."""
    import numpy as np

    splice = int(SPLICE_S * RATE)
    parts = [samples[a * RATE:b * RATE] for a, b in quiet if (b - a) * RATE > 2 * splice]
    if not parts:
        raise ContractError("the take has no quiet stretch to take the low band from")
    out = parts[0].copy()
    i = 1
    while len(out) < length:
        part = parts[i % len(parts)]
        i += 1
        ramp = np.sin(np.linspace(0, np.pi / 2, splice))[:, None]
        out[-splice:] = out[-splice:] * ramp[::-1] + part[:splice] * ramp
        out = np.concatenate([out, part[splice:]])
    return out[:length]


def swap_low_band(samples, windows: list[tuple[int, int]], quiet: list[tuple[int, int]]):
    """The samples with each window's band below BELOW_HZ swapped for the quiet
    stretches' (see the module doc)."""
    import numpy as np

    n, hop = 4096, 1024
    win = np.sqrt(np.hanning(n + 1)[:-1])        # analysis and synthesis both: Hann overall
    f = np.fft.rfftfreq(n, 1 / RATE)
    band = np.clip((ABOVE_HZ - f) / (ABOVE_HZ - BELOW_HZ), 0, 1)
    out = samples.copy()
    for a, b in windows:
        lo, hi = max(0, (a - 1) * RATE), min(len(samples), (b + 1) * RATE)
        # the sound's own start or end is no edge to fade at: there is nothing recorded to
        # fade back to there (a chop running to the last second stayed in it, beach-demo-003 C)
        opens, closes = lo > 0, hi < len(samples)
        donor = _donor(samples, [q for q in quiet if q[1] <= a or q[0] >= b], hi - lo)
        acc, norm = np.zeros((hi - lo, 2)), np.zeros(hi - lo)
        for i in range(0, hi - lo - n + 1, hop):
            t = (lo + i + n / 2) / RATE
            edge = min(t - a if opens else FADE_S, b - t if closes else FADE_S)
            weight = band * float(np.clip(edge / FADE_S, 0, 1))
            for c in (0, 1):
                mixed = (np.fft.rfft(samples[lo + i:lo + i + n, c] * win) * (1 - weight)
                         + np.fft.rfft(donor[i:i + n, c] * win) * weight)
                acc[i:i + n, c] += np.fft.irfft(mixed, n) * win
            norm[i:i + n] += win ** 2
        full = norm > 0.5                         # the stretch's own first and last few ms stay
        out[lo:hi][full] = acc[full] / norm[full][:, None]
    return out


def clean_take_audio(take_dir, *, ffmpeg: str | None = None) -> dict:
    """source/lane-1-audio-clean.wav from the take's lane 1 audio, and what was swapped in
    evidence/audio-clean.json. Seconds in the report are the videos' (stage time): the
    WAV starts the tape's hold earlier."""
    take_dir = Path(take_dir)
    source = take_dir / "source" / CANDIDATE
    if not source.is_file():
        raise ContractError(f"{source} is missing: export lane 1 first")
    hold_s = int(load_json(take_dir / "config" / "camera-tape.json")["source"]["hold_frames"]) / 30
    samples = read_audio(source, ffmpeg=ffmpeg)
    scores = chop_scores(samples)
    windows = chop_windows(scores)
    in_window = [any(a <= s < b for a, b in windows) for s in range(len(scores))]
    quiet = _runs([not w and sc < QUIET_SCORE for w, sc in zip(in_window, scores)])
    out = take_dir / "source" / CLEAN
    cleaned = swap_low_band(samples, windows, quiet) if windows else samples
    write_audio(cleaned, out, ffmpeg=ffmpeg)
    after = chop_scores(read_audio(out, ffmpeg=ffmpeg)) if windows else scores
    report = {
        "schema": "dm-gta-take-audio-clean/1",
        "source": {"path": str(source), "sha256": sha256_file(source)},
        "output": {"path": str(out), "sha256": sha256_file(out)},
        "below_hz": BELOW_HZ, "crossfade_hz": [BELOW_HZ, ABOVE_HZ], "fade_s": FADE_S,
        "swapped": [{"from_s": round(max(0.0, a - hold_s), 1), "to_s": round(b - hold_s, 1),
                     "chop_before": round(float(scores[a:b].max()), 1),
                     "chop_after": round(float(after[a:b].max()), 1)} for a, b in windows],
        "quiet_median": round(float(sorted(scores)[len(scores) // 2]), 1),
    }
    write_json_atomic(take_dir / "evidence" / REPORT, report)
    return report
