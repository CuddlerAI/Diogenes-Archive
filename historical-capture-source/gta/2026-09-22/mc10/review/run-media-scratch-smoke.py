"""Run tiny synthetic media tests with the pinned real FFmpeg; no GTA calls."""
from pathlib import Path
import argparse
import datetime
import json
import math
import shutil
import struct
import sys
import traceback
import wave
import zlib

sys.dont_write_bytecode = True
PACKAGE = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(PACKAGE))
from gta_capture import media
from gta_capture.errors import BackendError, ContractError
from gta_capture.jsonio import file_record
from gta_capture.preflight import DependencyManifest

def png(path, lane, frame):
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)
    width, height = 64, 48
    rows = b''.join(b'\0' + bytes(((lane * 43) % 256, (frame * 37) % 256, row * 5)) * width for row in range(height))
    path.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
                     + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    root = args.out.resolve()
    if not root.is_relative_to(PACKAGE / 'out'):
        raise RuntimeError('Smoke output must be inside task ignored out directory')
    if root.exists():
        raise RuntimeError('Smoke output exists; no overwrite allowed')
    root.mkdir(parents=True)
    report = {'schema': 'dm-gta-synthetic-media-scratch-smoke/1',
              'started_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'synthetic_test_only': True, 'real_gta_capture': False,
              'game_api_or_ui_calls': False, 'delivery_eligible': False,
              'output_root': str(root), 'commands': [], 'cases': {},
              'source_module': file_record(PACKAGE / 'gta_capture/media.py'),
              'smoke_helper': file_record(Path(__file__)),
              'manifest': file_record(args.manifest)}
    original_run, original_probe = media._run, media._probe_media
    try:
        manifest = DependencyManifest.load(args.manifest)
        ffmpeg, ffprobe = (manifest.pinned_tool_path(name) for name in ('ffmpeg', 'ffprobe'))
        report['tools'] = {'ffmpeg': file_record(Path(ffmpeg)), 'ffprobe': file_record(Path(ffprobe))}
        fixtures = root / 'synthetic-fixtures'
        fixtures.mkdir()
        artifacts, events, input_records = {}, {}, []
        for lane in range(1, 5):
            directory = fixtures / f'lane-{lane}'
            directory.mkdir()
            events[lane] = []
            for frame in range(6):
                path = directory / f'frame_{frame:06d}.png'
                png(path, lane, frame)
                record = file_record(path)
                input_records.append(record)
                events[lane].append({'output_path': record['path'], 'output_bytes': record['bytes'],
                                     'output_sha256': record['sha256']})
            artifacts[lane] = {'frame_count': 6, 'frame_pattern': str(directory / 'frame_%06d.png')}
        audio_path = fixtures / 'audio.wav'
        with wave.open(str(audio_path), 'wb') as wav:
            wav.setparams((2, 2, 48000, 0, 'NONE', 'not compressed'))
            wav.writeframes(b''.join(struct.pack('<hh', *([int(2000 * math.sin(2 * math.pi * 440 * i / 48000))] * 2))
                                     for i in range(9600)))
        audio_record = file_record(audio_path)
        input_records.append(audio_record)
        audio = {'audio_path': str(audio_path), 'audio_sha256': audio_record['sha256'],
                 'audio_bytes': audio_record['bytes']}
        report['synthetic_inputs'] = input_records
        identities = []
        current_case = 'success'

        def run(command):
            output = Path(command[-1]).resolve()
            if not output.is_relative_to(root):
                raise RuntimeError('Refuse media output outside synthetic smoke root')
            command = list(command)
            entry = {'case': current_case, 'argv': command, 'success': False}
            report['commands'].append(entry)
            if current_case == 'failure-second-mux' and output.name == 'video_2.mp4.muxing.mp4':
                command[command.index('-i') + 1] = str(root / 'intentionally-missing-test-input.mp4')
                entry['argv'] = command
                entry['intentional_failure'] = 'real FFmpeg receives absent synthetic test input'
            is_encode = '-an' in command
            before = media._file_identity(output) if is_encode else None
            if is_encode:
                lane = int(output.name.split('-')[1])
                scratch = output.parent
                if lane > 1:
                    assert not (scratch / f'lane-{lane-1}-frames').exists()
                    assert not (scratch / f'lane-{lane-1}-silent.mp4').exists()
                assert len(list(scratch.glob('lane-*-frames'))) == 1
            try:
                original_run(command)
            except Exception as exc:
                entry['error'] = str(exc)
                raise
            entry['success'] = True
            if is_encode:
                after = media._file_identity(output)
                identities.append({'case': current_case, 'path': str(output),
                                   'before': before, 'after': after, 'unchanged': before == after})
                assert before == after

        media._run = run
        def materialize(case):
            return media.materialize_videos(root / case / 'capture', artifacts, audio,
                                            ffmpeg=ffmpeg, ffprobe=ffprobe, lane_events=events)
        outputs = materialize(current_case)
        assert not (root / current_case / 'media-scratch').exists()
        verified = []
        for output in outputs:
            probe = original_probe(ffprobe, Path(output))
            media._verify_muxed_output(probe, n_frames=6, fps=30, expected_duration_s=.2)
            stream = next(s for s in probe['streams'] if s['codec_type'] == 'video')
            assert (stream['width'], stream['height']) == (64, 48)
            verified.append({'video': file_record(Path(output)), 'ffprobe': probe})
        report['cases']['success'] = {'ok': True, 'all_lane_scratch_released': True,
                                      'videos': verified}
        current_case = 'failure-second-mux'
        try:
            materialize(current_case)
        except BackendError as exc:
            assert 'intentionally-missing-test-input.mp4' in str(exc)
        else:
            raise AssertionError('Intentional second mux failure did not fail')
        failed_run = root / current_case
        assert (failed_run / 'capture/video_1.mp4').is_file()
        assert not (failed_run / 'capture/video_2.mp4').exists()
        assert not (failed_run / 'media-scratch/lane-1-frames').exists()
        assert len(list((failed_run / 'media-scratch/lane-2-frames').glob('*.png'))) == 6
        assert (failed_run / 'media-scratch/lane-2-silent.mp4').is_file()
        assert (failed_run / 'media-scratch/master-audio.wav').is_file()
        report['cases'][current_case] = {'ok': True, 'intentional_failure_verified': True,
                                         'earlier_video_retained': file_record(failed_run / 'capture/video_1.mp4'),
                                         'failed_lane_scratch_retained': True}
        for case, filename in (('existing-final', 'video_3.mp4'), ('existing-mux-temp', 'video_2.mp4.muxing.mp4')):
            current_case = case
            preserved = root / case / 'capture' / filename
            preserved.parent.mkdir(parents=True)
            shutil.copyfile(outputs[0], preserved)
            before = file_record(preserved)
            commands_before = len(report['commands'])
            try:
                materialize(case)
            except ContractError as exc:
                assert 'output already exists' in str(exc)
            else:
                raise AssertionError('Preexisting output was not refused')
            assert file_record(preserved) == before
            assert len(report['commands']) == commands_before
            assert not (root / case / 'media-scratch').exists()
            report['cases'][case] = {'ok': True, 'untouched': before, 'ffmpeg_not_started': True}
        assert all(file_record(Path(item['path'])) == item for item in input_records)
        report['synthetic_input_bytes_and_paths_unchanged'] = True
        report['ffmpeg_silent_output_identity_observations'] = identities
        report['status'] = 'PASS'
    except Exception:
        report['status'] = 'FAIL'
        report['error'] = traceback.format_exc()
    finally:
        media._run, media._probe_media = original_run, original_probe
        report['finished_at_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        receipt = root / 'smoke-receipt.json'
        with receipt.open('x', encoding='utf-8', newline='\n') as handle:
            json.dump(report, handle, ensure_ascii=False, indent=2)
            handle.write('\n')
        print(json.dumps({'status': report['status'], 'receipt': file_record(receipt),
                          'cases': {k:v['ok'] for k,v in report['cases'].items()},
                          'error': report.get('error')}, ensure_ascii=False, indent=2))
    return 0 if report['status'] == 'PASS' else 1

if __name__ == '__main__':
    raise SystemExit(main())
