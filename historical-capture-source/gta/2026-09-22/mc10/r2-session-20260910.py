"""Scoped R2 session. Secrets exist only in captured memory and child environment."""
import argparse
import concurrent.futures
import datetime as dt
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

BASE = Path(__file__).resolve().parent
OLD = 'r2:diogenes-tempbucket/pr852-gta5-legacy-pr405-non-trainable-20260828-115407-7c6c8ac1/poc061'
GCLOUD = r'C:\Users\researcher\CloudSDK\google-cloud-sdk\bin\gcloud.cmd'

def environment():
    values = {}
    for key, secret in [('ACCOUNT','r2-training-account-id'), ('ACCESS_KEY_ID','r2-training-access-key-id'), ('SECRET_ACCESS_KEY','r2-training-secret-access-key')]:
        result = subprocess.run([GCLOUD, 'secrets', 'versions', 'access', 'latest', '--secret='+secret, '--project=hakkolite', '--quiet'], capture_output=True, timeout=90)
        if result.returncode:
            raise RuntimeError('Credential retrieval failed for '+secret+'; no secret output logged')
        values[key] = result.stdout.decode('utf-8').strip()
        if not values[key] or '\n' in values[key]:
            raise RuntimeError('Invalid credential value; no secret output logged')
    env = os.environ.copy()
    env.update(RCLONE_CONFIG_R2_TYPE='s3', RCLONE_CONFIG_R2_PROVIDER='Cloudflare', RCLONE_CONFIG_R2_REGION='auto', RCLONE_CONFIG_R2_ACCESS_KEY_ID=values['ACCESS_KEY_ID'], RCLONE_CONFIG_R2_SECRET_ACCESS_KEY=values['SECRET_ACCESS_KEY'], RCLONE_CONFIG_R2_ENDPOINT='https://'+values['ACCOUNT']+'.r2.cloudflarestorage.com')
    return env

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('mode', choices=['verify-historical', 'restore-historical-samples', 'restore-render', 'upload'])
    parser.add_argument('--source', type=Path)
    parser.add_argument('--destination')
    parser.add_argument('--report-dir', type=Path)
    parser.add_argument('--resume', action='store_true')
    parser.add_argument('--tuned', action='store_true', help='Measured 32MiB part stalls: use bounded 8MiB parts and 12 transfers')
    parser.add_argument('--http1', action='store_true', help='Task-scoped HTTP/1 comparison for observed HTTP/2 transfer stalls')
    parser.add_argument('--local-http-proxy', action='store_true', help='Use the existing local mixed-port HTTP inbound for this task process only')
    args = parser.parse_args()
    rclone = shutil.which('rclone')
    if not rclone:
        raise RuntimeError('rclone missing')
    env = environment()
    if args.http1:
        env['RCLONE_S3_DISABLE_HTTP2'] = 'true'
    if args.local_http_proxy:
        env['HTTPS_PROXY'] = 'http://127.0.0.1:7897'
        env['HTTP_PROXY'] = 'http://127.0.0.1:7897'
    report_dir = BASE / 'review' / 'historical-r2-verification-20260910-01'
    if args.mode == 'restore-render':
        import hashlib
        if not args.report_dir:
            raise RuntimeError('Upload report directory required')
        upload = json.loads((args.report_dir/'upload-report.json').read_text(encoding='utf-8'))
        source = Path(upload['source']).resolve(strict=True)
        if source.parent != Path(r'C:\capture\gta\replus-output') or source.name not in ['render_0058','render_0059','render_0060','render_0061','render_0052','render_0053']:
            raise RuntimeError('Not an allowed render')
        destination = 'r2:diogenes-tempbucket/gta-mc-demos-20260910/archives/raw/'+source.name
        if upload['destination'] != destination or upload['status'] != 'verified' or upload['copy_exit_code'] != 0 or upload['check_exit_code'] != 0 or upload['source_before'] != upload['source_after']:
            raise RuntimeError('Verified upload required')
        entries = [json.loads(line) for line in (BASE/'review/raw-archive-20260910-01'/source.name/'files.sha256.jsonl').read_text(encoding='utf-8').splitlines()]
        expected = {row['relative_path']:row for row in entries}
        sample_dir = args.report_dir/'restored-samples'
        sample_dir.mkdir(exist_ok=False)
        restored = []
        for frame in [0,900,1799]:
            name = 'frame_%06d.png' % frame
            row = expected[name]
            remote = destination+'/'+name
            target = sample_dir/name
            result = subprocess.run([rclone,'copyto',remote,str(target),'--retries','3'],env=env,capture_output=True)
            if result.returncode:
                raise RuntimeError('Restore sample download failed; no credential output logged')
            with target.open('rb') as handle:
                actual = hashlib.file_digest(handle,'sha256').hexdigest()
            restored.append({'original_path':row['original_path'],'remote_object':remote,'restored_path':str(target.resolve()),'original_sha256':row['sha256'],'restored_sha256':actual,'bytes':target.stat().st_size,'match':actual==row['sha256'] and target.stat().st_size==row['bytes']})
            (args.report_dir/'restore-results.json').write_text(json.dumps(restored,indent=2)+'\n',encoding='utf-8')
            if not restored[-1]['match']:
                raise RuntimeError('Restored sample mismatch')
        print(json.dumps({'render':source.name,'restored_samples':len(restored),'all_sha256_match':True}),flush=True)
        return 0
    if args.mode == 'upload':
        if not args.source or not args.destination or not args.report_dir:
            raise RuntimeError('Exact source, destination and report directory required')
        if not args.destination.startswith('r2:diogenes-tempbucket/gta-mc-demos-20260910/') or any(p in args.destination.split('/') for p in ['.','..']):
            raise RuntimeError('Outside proposed task scope; requires explicit scope update')
        repo = BASE.parents[3]
        helper = repo / '.agents/skills/upload-r2-high-concurrency/scripts/upload_r2.py'
        command = [sys.executable, '-X', 'utf8', str(helper), '--source', str(args.source), '--destination', args.destination, '--report-dir', str(args.report_dir), '--rclone', rclone, '--memory-budget-mib', '1024']
        if args.tuned:
            command.extend(['--transfers','12','--checkers','32','--upload-concurrency','2','--chunk-size-mib','8'])
        if args.resume:
            command.append('--resume')
        return subprocess.run(command,env=env).returncode
    if args.mode == 'restore-historical-samples':
        import hashlib
        samples_dir = report_dir / 'restored-samples'
        samples_dir.mkdir(exist_ok=False)
        restored = []
        def digest(path):
            with path.open('rb') as handle:
                return hashlib.file_digest(handle,'sha256').hexdigest()
        for lane in range(1,5):
            for frame in [0,225,449]:
                name = 'frame_%06d.png' % frame
                source = Path(r'C:\capture\gta\replus-output') / ('render_%04d' % (36+lane)) / name
                destination = OLD + '/raw/lane' + str(lane) + '/' + name
                target = samples_dir / ('lane%d-%s'%(lane,name))
                expected = digest(source)
                result = subprocess.run([rclone,'copyto',destination,str(target),'--retries','3'],env=env,capture_output=True)
                if result.returncode:
                    raise RuntimeError('Sample restore failed; no credential output logged')
                actual = digest(target)
                row = {'original_path':str(source),'remote_object':destination,'restored_path':str(target),'original_sha256':expected,'restored_sha256':actual,'bytes':target.stat().st_size,'match':actual==expected}
                restored.append(row)
                (report_dir/'restore-results.json').write_text(json.dumps(restored,indent=2)+'\n',encoding='utf-8')
                if not row['match']:
                    raise RuntimeError('Restored sample mismatch')
        print(json.dumps({'restored_samples':len(restored),'bytes':sum(r['bytes'] for r in restored),'all_sha256_match':all(r['match'] for r in restored)}),flush=True)
        return 0
    report_dir.mkdir(exist_ok=False)
    def verify(lane):
        source = Path(r'C:\capture\gta\replus-output') / ('render_%04d' % (36+lane))
        destination = OLD + '/raw/lane' + str(lane)
        report = {'source':str(source), 'destination':destination, 'started_at':dt.datetime.now(dt.timezone.utc).isoformat()}
        files = [p for p in source.rglob('*') if p.is_file()]
        report['local'] = {'count':len(files), 'bytes':sum(p.stat().st_size for p in files)}
        result = subprocess.run([rclone,'size',destination,'--json'],env=env,capture_output=True,text=True)
        report['size_exit_code'] = result.returncode
        if result.returncode:
            report['status']='remote_size_failed'
        else:
            report['remote'] = json.loads(result.stdout)
            with (report_dir / ('lane%d.log'%lane)).open('w',encoding='utf-8') as log:
                check = subprocess.run([rclone,'check',str(source),destination,'--checkers','4','--combined',str(report_dir/('lane%d-check.txt'%lane)), '--log-level','INFO'], env=env,stdout=log,stderr=subprocess.STDOUT)
            report['check_exit_code'] = check.returncode
            report['status'] = 'check_complete' if check.returncode == 0 else 'check_failed'
        (report_dir/('lane%d.json'%lane)).write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8')
        print(json.dumps(report),flush=True)
        return report
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(verify, range(1,5)))
    return 0 if all(r.get('check_exit_code')==0 for r in results) else 1

if __name__ == '__main__':
    raise SystemExit(main())
