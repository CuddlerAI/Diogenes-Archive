import sys,json,math
from pathlib import Path
from types import SimpleNamespace
root=Path(__file__).resolve().parents[3]
sys.path[:0]=[str(root/'trajgen'),str(root/'qc')]
from trajgen.pink_honesty import enforce
from trajgen.mathutil import smoothstep5
from trajgen.metrics import physics,decouple,pink_honesty_check
from trajgen.covis import CamView,evaluate
from trajgen.scene import Scene,synthetic
base=Path(__file__).parent/'plan-clip'
plan=json.loads((base/'plan.json').read_text())
old=[json.loads((base/f'action_camera_{i}.json').read_text()) for i in range(1,5)]
anchors=plan['anchors']
def anc(t):
    idx=min(int(t/5),len(anchors)-2)
    a,b=anchors[idx:idx+2]
    u=smoothstep5((t-a['t'])/(b['t']-a['t']))
    return [a['p'][j]+u*(b['p'][j]-a['p'][j]) for j in range(3)]
for speed in (.25,.3,.4,.5):
    for corrections in (False,True):
        tracks={}
        for lane,records in enumerate(old,1):
            positions=[]
            for idx,rec in enumerate(records):
                t=idx/30
                a=anc(t)
                desired=[anchors[0]['p'][0],anchors[0]['p'][1],anchors[0]['p'][2]+speed*t]
                positions.append(tuple(rec['camera_position'][j]+desired[j]-a[j] for j in range(3)))
            quats=[r['camera_rotation_quaternion'] for r in records]
            moves=[SimpleNamespace(**m) for m in plan['cameras'][lane-1]['segments']]
            if corrections:positions=enforce(positions,quats,moves,30,0)
            tracks[lane]=(positions,quats)
            print(speed,corrections,lane,'phys',physics(positions,quats,1/30).violations(lane==4),'pink',pink_honesty_check(positions,quats,moves,30),flush=True)
        print('decouple',decouple(tracks,1/30).to_dict(),flush=True)
        times=[i/2 for i in range(120)]
        views={i:[CamView(tracks[i][0][k*15],tracks[i][1][k*15],**old[i-1][k*15]['camera_intrinsics'],width=1920,height=1080) for k in range(120)] for i in range(1,4)}
        scene=Scene('ground-proxy','open_plaza',(-100,0,-100),(100,60,100),0)
        cv=evaluate(scene,views,times)
        print('covis',cv.ratio,flush=True)

