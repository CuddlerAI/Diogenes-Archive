"""deliver_qc.py <group dir> <delivery dir> <qc dir>: the group's delivery package into <delivery dir>
(stage-group-deliver; a package of that name must not be there: rename the old one aside first, supersede.ps1),
then worldline_qc on it into <qc dir>\\qc.html and qc.json. --qc-only skips the delivery. Run on the capture PC."""
import glob, json, os, subprocess, sys
GC = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))   # gta-capture
ENGINE = os.path.dirname(GC)
env = dict(os.environ, PYTHONIOENCODING="utf-8", PYTHONUTF8="1")
group, out, qc = (os.path.abspath(a) for a in sys.argv[1:4])
if "--qc-only" not in sys.argv[4:]:
    r = subprocess.run([sys.executable, "-m", "gta_capture", "stage-group-deliver", "--group", group, "--out", out],
                       cwd=GC, env=env, capture_output=True, text=True, encoding="utf-8")
    print(r.stdout[-3000:]); print(r.stderr[-3000:])
    if r.returncode:
        sys.exit(r.returncode)
name = json.load(open(os.path.join(group, "group.json"), encoding="utf-8"))["delivery_name"]
pkg = os.path.join(out, name)
assert os.path.isdir(pkg), pkg
os.makedirs(qc, exist_ok=True)
r = subprocess.run([sys.executable, "-m", "worldline_qc", "check-group", pkg, "--expect-duration-s", "120",
                    "--html", os.path.join(qc, "qc.html"), "--json", os.path.join(qc, "qc.json")],
                   cwd=os.path.join(ENGINE, "qc"), env=env, capture_output=True, text=True, encoding="utf-8")
print(r.stdout[-4000:]); print(r.stderr[-2000:]); print("qc exit", r.returncode)
