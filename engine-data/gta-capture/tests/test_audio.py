import math
import struct
import tempfile
import unittest
import wave
from pathlib import Path

from helpers import MODULE_ROOT  # noqa: F401 - importing helpers installs the package root
from gta_capture.audio import normalize_replus_wav, validate_wav
from gta_capture.errors import ContractError
from gta_capture.jsonio import sha256_file


class AudioValidationTests(unittest.TestCase):
    @staticmethod
    def _write_pcm(path: Path, values: list[int], *, rate: int = 48000,
                   channels: int = 2, width: int = 2) -> None:
        with wave.open(str(path), "wb") as output:
            output.setnchannels(channels)
            output.setsampwidth(width)
            output.setframerate(rate)
            samples = bytearray()
            for value in values:
                if width == 2:
                    samples.extend(struct.pack("<" + "h" * channels,
                                               *([value] * channels)))
                elif width == 1:
                    samples.extend(bytes([max(0, min(255, value + 128))]) * channels)
                else:
                    raise AssertionError("test helper only supports 8/16-bit PCM")
            output.writeframes(samples)

    @staticmethod
    def _write_float32(path: Path, values: list[float], *, rate: int = 48000,
                       channels: int = 2) -> None:
        samples = b"".join(
            struct.pack("<" + "f" * channels, *([value] * channels))
            for value in values)
        path.write_bytes(
            b"RIFF" + struct.pack("<I", 36 + len(samples)) + b"WAVEfmt "
            + struct.pack("<IHHIIHH", 16, 3, channels, rate,
                          rate * channels * 4, channels * 4, 32)
            + b"data" + struct.pack("<I", len(samples)) + samples)

    def test_replus_float32_is_atomically_normalized_to_pcm_s16le(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "replus.wav"
            rate = 48000
            self._write_float32(path, [
                0.25 * math.sin(2 * math.pi * 440 * index / rate)
                for index in range(rate)
            ])
            source_sha256 = sha256_file(path)
            normalization = normalize_replus_wav(path)
            report = validate_wav(path, expected_duration_s=1.0)
            with wave.open(str(path), "rb") as normalized:
                normalized_format = (
                    normalized.getnchannels(), normalized.getframerate(),
                    normalized.getsampwidth(), normalized.getnframes())

        self.assertTrue(normalization["converted"])
        self.assertEqual(normalization["source_format_tag"], 3)
        self.assertEqual(normalization["source_audio_sha256"], source_sha256)
        self.assertNotEqual(normalization["audio_sha256"], source_sha256)
        self.assertEqual(normalized_format, (2, 48000, 2, 48000))
        self.assertEqual(report["encoding"], "pcm_s16le")
        self.assertTrue(report["continuous"])
        self.assertTrue(report["non_silent"])

    def test_non_finite_float32_fails_without_replacing_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "non-finite.wav"
            self._write_float32(path, [float("nan")] * 48000)
            source_sha256 = sha256_file(path)
            with self.assertRaisesRegex(ContractError, "non-finite"):
                normalize_replus_wav(path)
            self.assertEqual(sha256_file(path), source_sha256)

    def test_pcm_duration_and_non_silence_are_measured(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "audio.wav"
            rate = 48000
            self._write_pcm(path, [
                int(4000 * math.sin(2 * math.pi * 440 * index / rate))
                for index in range(rate)
            ], rate=rate)
            report = validate_wav(path, expected_duration_s=1.0)
            expected_bytes = path.stat().st_size
        self.assertTrue(report["continuous"])
        self.assertTrue(report["non_silent"])
        self.assertTrue(report["active_coverage"])
        self.assertTrue(report["max_silence_ok"])
        self.assertTrue(report["clipping_ok"])
        self.assertFalse(report["radio_off"])
        self.assertIsNone(report["radio_off_evidence"])
        self.assertEqual(report["audio_bytes"], expected_bytes)
        self.assertEqual(report["pcm_frames_read"], rate)

    def test_single_transient_followed_by_silence_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "transient.wav"
            values = [0] * 48000
            values[100] = 12000
            self._write_pcm(path, values)
            report = validate_wav(path, expected_duration_s=1.0)
        self.assertTrue(report["duration_integrity"])
        self.assertFalse(report["active_coverage"])
        self.assertFalse(report["max_silence_ok"])
        self.assertFalse(report["continuous"])
        self.assertFalse(report["non_silent"])

    def test_heavily_clipped_audio_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "clipped.wav"
            self._write_pcm(path, [32767 if index % 2 else -32768
                                   for index in range(48000)])
            report = validate_wav(path, expected_duration_s=1.0)
        self.assertTrue(report["active_coverage"])
        self.assertFalse(report["clipping_ok"])
        self.assertFalse(report["continuous"])

    def test_wrong_pcm_format_is_rejected(self):
        fixtures = ((44100, 2, 2), (48000, 1, 2), (48000, 2, 1))
        for rate, channels, width in fixtures:
            with self.subTest(rate=rate, channels=channels, width=width):
                with tempfile.TemporaryDirectory() as tmp:
                    path = Path(tmp) / "wrong-format.wav"
                    self._write_pcm(path, [1000] * rate, rate=rate,
                                    channels=channels, width=width)
                    with self.assertRaisesRegex(
                            ContractError, "PCM s16le, 48 kHz, stereo"):
                        validate_wav(path, expected_duration_s=1.0)


if __name__ == "__main__":
    unittest.main()
