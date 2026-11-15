"""Read-only integrity diagnostic for real Story-to-Editor transfer; no gates."""
from __future__ import annotations
import argparse
import hashlib
import io
import json
import math
import os
import struct
import sys
from datetime import datetime, timezone
from pathlib import Path
from PIL import Image

HERE = Path(__file__).resolve().parent
CAPTURE = HERE.parents[3]
BASE = CAPTURE / "out/mc2-20260908"
RUN = BASE / "story-editor-transfer-draft-01/run"
PREPARED = BASE / "story-editor-transfer-prepared-01"
sys.path.insert(0, str(CAPTURE))
from gta_capture.evidence import validate_alignment


def sha(data): return hashlib.sha256(data).hexdigest()
def norm(path): return os.path.normcase(str(Path(path).resolve()))
def samepath(a, b): return norm(a) == norm(b)
def distance(a, b): return math.sqrt(sum((float(x)-float(y))**2 for x,y in zip(a,b)))
def qangle(a,b):
    na,nb=math.sqrt(sum(x*x for x in a)), math.sqrt(sum(x*x for x in b))
    a,b=[x/na for x in a],[x/nb for x in b]
    chord=min(distance(a,b),distance(a,[-x for x in b]))
    return math.degrees(4*math.asin(min(1.0,chord/2)))


class Review:
    def __init__(self): self.files={};self.checks={};self.failures=[]
    def check(self,name,condition,detail=None):
        counts=self.checks.setdefault(name,{"PASS":0,"FAIL":0})
        counts["PASS" if condition else "FAIL"]+=1
        if not condition:self.failures.append({"check":name,"detail":detail})
        return condition
    def read(self,path,expected=None):
        path=Path(path).resolve();data=path.read_bytes()
        record={"path":str(path),"bytes":len(data),"sha256":sha(data)}
        if expected:
            self.check("file_binding",record["bytes"]==expected["bytes"] and record["sha256"]==expected["sha256"] and samepath(path,expected.get("path",path)),str(path))
        if str(path) in self.files:self.check("input_unchanged_during_review",self.files[str(path)]==record,str(path))
        self.files[str(path)]=record
        return data
    def json(self,path,expected=None):return json.loads(self.read(path,expected))
    def png(self,record):
        data=self.read(record["path"],record)
        self.check("png_header_dimensions",data[:8]==b"\x89PNG\r\n\x1a\n" and struct.unpack(">II",data[16:24])==(1920,1080),record["path"])
        with Image.open(io.BytesIO(data)) as im:
            self.check("png_decodes",im.format=="PNG" and im.size==(1920,1080),record["path"])
            im.verify()
        return self.files[str(Path(record["path"]).resolve())]
    def camera(self,a,b,context):
        p=distance(a["position"],b["position"]); q=qangle(a["quaternion"],b["quaternion"])
        self.check("camera_position_tolerance",p<=0.0005,{"context":context,"error_world_units":p})
        self.check("camera_rotation_tolerance",q<=0.005,{"context":context,"error_degrees":q})
        self.check("camera_unit_quaternion",abs(math.sqrt(sum(x*x for x in a["quaternion"]))-1)<=0.001,context)
        return p,q


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out",type=Path,required=True)
    parser.add_argument("--lanes",type=int,nargs="+",choices=[1,2,3,4],default=[1,2,3,4])
    parser.add_argument("--state-snapshot",type=Path,action="append",default=[])
    args=parser.parse_args();output=args.out.resolve()
    if output.exists() or not output.is_relative_to(HERE):raise ValueError("New output must be inside the independent review directory")
    lanes_to_check=sorted(set(args.lanes));r=Review();rows_by_lane={};lane_reports=[];selected=[];pending=[]
    prepare=r.json(PREPARED/"prepare-receipt.json")
    tape=r.json(prepare["camera_tape"]["path"],prepare["camera_tape"])
    r.read(prepare["native_tape"]["path"],prepare["native_tape"])
    for name,key in (("camera-tape.json","camera_tape"),("camera-tape.bin","native_tape")):
        data=r.read(RUN/"config"/name)
        r.check("run_tape_copy",sha(data)==prepare[key]["sha256"],name)
    manifest=r.json(prepare["dependency_manifest"]["path"],prepare["dependency_manifest"])
    run_manifest=r.json(RUN/"config/dependency-manifest.json")
    r.check("installed_manifest_semantic_copy",manifest==run_manifest)
    index_path=PREPARED/"comparison-index-plan.json";index=r.json(index_path)
    record_path=RUN/"evidence/native-record-result.json";record=r.json(record_path);master=record["master"]
    replay_sha=master["replay_sha256"];pid=record["hello"]["process_id"];master_gen=prepare["recording_controller_generation"]
    r.check("real_master_envelope",record["ok"] is True and master["mock"] is False and master["recording_start_verified"] is True)
    r.check("master_pid_preparation",pid==prepare["recording_process_id"] and record["hello"]["controller_generation"]==master_gen)
    r.check("master_sha_references",replay_sha==prepare["source_replay_sha256"]==tape["source"]["replay_sha256"]==index["source_replay_sha256"])
    r.check("master_replay_bytes",sha(r.read(master["replay_path"]))==replay_sha)
    r.check("master_original_clip_bytes",sha(r.read(master["source_clip_path"]))==replay_sha)
    telemetry_data=r.read(master["player_telemetry_path"])
    r.check("master_telemetry_hash",sha(telemetry_data)==master["player_telemetry_sha256"])
    telemetry=[json.loads(line) for line in telemetry_data.splitlines() if line.strip()]
    r.check("complete_window",tape["n_frames"]==prepare["frames_per_lane"]==index["complete_frames_per_lane"]==master["player_observation_count"]==len(telemetry)==150 and tape["fps_num"]==30 and tape["fps_den"]==1)
    r.check("diagnostic_not_calibrated",tape["source"]["kind"]=="story_editor_transfer_diagnostic" and tape["source"]["intrinsics_calibrated"] is False and tape["source"]["editor_equivalence_verified"] is False and tape["source"]["delivery_eligible"] is False)
    r.check("selected_plan_frames",sorted((s["lane"],s["planned_output_frame"]) for s in index["selected_stills"])==[(lane,frame) for lane in range(1,5) for frame in (45,120)])
    holds={}
    for lane in range(1,5):
        for hold_number,hold in enumerate(tape["source"]["story_holds"][str(lane)],1):
            context=f"lane {lane} Story hold {hold_number}"
            files=hold["files"]
            observation=r.json(files["observation.json"]["path"],files["observation.json"])
            request=r.json(files["request.json"]["path"],files["request.json"])
            runtime=r.json(files["runtime-before.json"]["path"],files["runtime-before.json"])
            png=r.png(files["point.png"])
            original_dir=Path(observation["output_path"]).parent
            for filename,binding in files.items():
                original=original_dir/filename
                r.check("story_original_frozen_bytes",sha(r.read(original))==binding["sha256"],f"{context}: {filename}")
            r.check("story_actual_draft",observation["schema"]=="dm-gta-story-calibration-frame/1" and observation["mock"] is False and observation["marker_mode"]==request["marker_mode"]=="none" and observation["approval_status"]=="draft" and observation["editor_equivalence_verified"] is False,context)
            r.check("story_png_observation_hash",png["sha256"]==observation["output_sha256"] and png["bytes"]==observation["output_bytes"],context)
            r.check("story_request_hash",observation["request_sha256"]==files["request.json"]["sha256"],context)
            r.check("story_master_process_binding",runtime["process_id"]==runtime["master_process_id"]==pid and runtime["master_record_generation"]==master_gen and runtime["master_replay_sha256"]==runtime["master_source_sha256"]==replay_sha and runtime["record_binding_ready"] is True and runtime["story_mode"] is True and runtime["online_session"] is False,context)
            r.camera(observation["camera_before"],hold["camera"],context)
            r.camera(observation["camera_after"],observation["camera_before"],context+" stability")
            r.check("story_fov_stability",abs(observation["camera_before"]["fov_deg"]-observation["camera_after"]["fov_deg"])<=0.001,context)
            transport=observation["transport"]
            r.check("story_transport_bracket",transport["protocol"]==18 and transport["editor_writer_fence"] is False and transport["request_id"]==transport["pixel_ready_id"]==transport["observer_release_id"]==transport["ack_id"]>0 and transport["generation_echo"]==transport["pixel_ready_generation_echo"]>0,context)
            plan_still=next(s for s in index["selected_stills"] if s["lane"]==lane and s["hold"]==hold_number)
            r.check("selected_story_plan_binding",plan_still["story_png"]==files["point.png"],context)
            r.check("selected_still_after_settle",plan_still["planned_output_frame"]-hold["start_frame"]>=12 and plan_still["planned_output_frame"]<hold["end_frame_exclusive"],context)
            holds[(lane,hold_number)]={"source":hold,"observation":observation,"story_png":png,"plan":plan_still}

    requests=set();generations=set();paths=set();project_names=set();all_profiles=set();sessions=set();last_global_generation=None
    for lane in lanes_to_check:
        result_path=RUN/f"evidence/native-lane-{lane}-result.json"
        if not result_path.exists():
            pending.append(lane);lane_reports.append({"lane":lane,"status":"PENDING","reason":"No completed collect result; in-progress events intentionally not read"});continue
        before_failures=len(r.failures)
        try:
            arm=r.json(RUN/f"evidence/native-lane-{lane}-arm.json");result=r.json(result_path)
            r.check("arm_collect_ok",arm["ok"] is True and arm["lane"]["armed"] is True and result["ok"] is True and result["lane"]["mock"] is False,lane)
            session=arm["lane"]["editor_session_id"]
            r.check("arm_collect_session",session==result["lane"]["editor_session_id"] and result["lane"]["lane"]==arm["lane"]["lane"]==lane,lane)
            r.check("unique_lane_session",session not in sessions,lane);sessions.add(session)
            for phase,payload in (("arm",arm),("collect",result)):
                for kind in ("hello","probe"):
                    state=payload[kind]
                    r.check("master_session_pid_binding",state["process_id"]==state["master_process_id"]==pid and state["master_record_generation"]==master_gen and state["master_replay_sha256"]==state["master_source_sha256"]==replay_sha and state["record_binding_ready"] is True and state["record_binding_error"] is None and state["story_mode"] is True and state["online_session"] is False,{"lane":lane,"phase":phase,"kind":kind})
                r.check("hello_probe_generation_agree",payload["hello"]["controller_generation"]==payload["probe"]["controller_generation"],{"lane":lane,"phase":phase})
            r.check("controller_reentry_monotonic",result["hello"]["controller_generation"]>=arm["hello"]["controller_generation"]>=master_gen,lane)
            event_path=Path(result["lane"]["frame_events_path"])
            r.check("event_path_lane",samepath(event_path,RUN/f"evidence/frame-events-lane{lane}.jsonl"),lane)
            event_bytes=r.read(event_path)
            r.check("collect_event_hash",sha(event_bytes)==result["lane"]["frame_events_sha256"],lane)
            raw_lines=event_bytes.splitlines();events=[json.loads(line) for line in raw_lines]
            r.check("all_150_frames",len(events)==result["lane"]["frame_count"]==150,lane)
            rows_by_lane[lane]=events
            max_errors={"position_world_units":0.0,"normalized_quaternion_deg":0.0,"fov_control_intrinsics_px":0.0,"replay_clock_ms":0.0}
            previous_request=previous_sequence=previous_heartbeat=None
            for frame,event in enumerate(events):
                context={"lane":lane,"frame":frame};sample=tape["lanes"][str(lane)]["samples"][frame]
                binding=event["editor_binding"];camera=event["final_camera"]
                hold=holds[(lane,1 if frame<75 else 2)]
                r.check("real_event_envelope",event["schema"]=="dm-gta-frame/1" and event["mock"] is False and event["render_succeeded"] is True and event["lane"]==lane and event["output_frame"]==event["replay_frame"]==sample["frame"]==frame,context)
                r.check("exact_source_clock",event["source_t_ns"]==sample["source_t_ns"]==round(frame*1_000_000_000/30) and event["replay_sha256"]==replay_sha,context)
                clock_error=abs((event["replay_time_ms"]-events[0]["replay_time_ms"])-event["source_t_ns"]/1e6)
                r.check("real_replay_clock",math.isfinite(event["replay_time_ms"]) and event["replay_time_ms"]>=0 and clock_error<=1,context)
                r.check("master_telemetry_origin",event["player_telemetry_frame"]==telemetry[frame]["frame"]==frame and event["player_telemetry_sha256"]==master["player_telemetry_sha256"] and samepath(event["player_telemetry_path"],master["player_telemetry_path"]) and event["player_observation_t_ns"]==telemetry[frame]["observation_t_ns"] and event["entity_state_hash"]==telemetry[frame]["entity_state_hash"] and event["player_state"]==telemetry[frame]["player_state"] and event["player_observation_method"]==telemetry[frame]["player_observation_method"],context)
                r.check("player_sample_timing",abs(event["player_observation_t_ns"]-event["source_t_ns"])<=20_000_000,context)
                p,q=r.camera(camera,sample,context);r.camera(sample,hold["source"]["camera"],{"lane":lane,"frame":frame,"against":"real Story camera"})
                k_error=max(abs(camera["intrinsics"][key]-sample["intrinsics"][key]) for key in ("fx","fy","cx","cy"))
                control_f=540/math.tan(math.radians(hold["observation"]["camera_before"]["fov_deg"])/2)
                r.check("fov_control_k_only",k_error<=0.1 and abs(sample["intrinsics"]["fx"]-control_f)<=1e-8 and abs(sample["intrinsics"]["fy"]-control_f)<=1e-8 and sample["intrinsics"]["cx"]==960 and sample["intrinsics"]["cy"]==540 and camera["focal_mm"]==tape["lanes"][str(lane)]["focal_mm"],context)
                r.check("actual_generation_fence",event["capture_protocol"]==18 and event["editor_identity_protocol"]==4 and event["capture_fence"]=="present_pixel_ready_final_writer_generation_pose_release_ack" and binding["method"]=="replus-present-plan-v2" and event["pixel_ready_final_camera_generation"]==binding["final_camera_generation"]>0,context)
                r.check("actual_editor_identity",binding["session_id"]==session and binding["clip_count"]==1 and binding["clip_index"]==0 and binding["clip_identity"]==master["source_clip_identity"] and bool(binding["project_name"]),context)
                gen,req,seq,hb=(binding[k] for k in ("final_camera_generation","capture_request_id","identity_sequence","identity_heartbeat"))
                r.check("unique_generation_request",gen not in generations and req not in requests and gen>0 and req>0,context)
                r.check("generation_continuity",last_global_generation is None or gen==last_global_generation+1,context)
                r.check("identity_monotonic",seq>=2 and seq%2==0 and hb>=1 and (previous_request is None or req>previous_request) and (previous_sequence is None or seq>previous_sequence) and (previous_heartbeat is None or hb>previous_heartbeat),context)
                generations.add(gen);requests.add(req);last_global_generation=gen
                previous_request,previous_sequence,previous_heartbeat=req,seq,hb
                r.check("render_profile_binding",binding["render_profile_sha256"]==result["lane"]["render_profile_sha256"] and samepath(binding["normalized_output_root"],result["lane"]["normalized_output_root"]),context)
                all_profiles.add(binding["render_profile_sha256"]);project_names.add(binding["project_name"])
                png_path=Path(event["output_path"]).resolve()
                r.check("raw_png_path_unique_and_contained",str(png_path) not in paths and png_path.is_relative_to(Path(binding["normalized_output_root"]).resolve()) and samepath(png_path,result["lane"]["frame_pattern"]%frame),context)
                paths.add(str(png_path))
                png_record={"path":str(png_path),"bytes":event["output_bytes"],"sha256":event["output_sha256"]}
                r.png(png_record)
                max_errors={k:max(max_errors[k],v) for k,v in zip(max_errors,(p,q,k_error,clock_error))}
                if frame in (45,120):
                    r.check("selected_plan_actual_frame",hold["plan"]["planned_output_frame"]==frame,context)
                    selected.append({"lane":lane,"hold":1 if frame==45 else 2,"output_frame":frame,"source_t_ns":event["source_t_ns"],
                                     "story_png":hold["story_png"],"editor_png":png_record,
                                     "event_file":r.files[str(event_path.resolve())],"event_line_1_based":frame+1,
                                     "event_line_sha256_without_newline":sha(raw_lines[frame]),
                                     "actual_event":event,"camera_control_matches_story":p<=.0005 and q<=.005 and k_error<=.1,
                                     "pixel_registration_status":"NOT_PERFORMED_BY_INTEGRITY_REVIEW","editor_equivalence_verified":False})
            lane_reports.append({"lane":lane,"status":"PASS" if len(r.failures)==before_failures else "FAIL","frame_count":len(events),"max_errors":max_errors,
                                 "session_id":session,"arm_controller_generation":arm["hello"]["controller_generation"],"collect_controller_generation":result["hello"]["controller_generation"],
                                 "master_record_generation":master_gen,"process_matches_recording":result["hello"]["process_id"]==pid,
                                 "collect_editor_project_binding_was_null":result["lane"]["editor_project_binding"] is None,
                                 "project_binding_source":"Every actual frame event editor_binding, checked against arm/collect session and master clip identity"})
            print(f"Read-only lane {lane}: {lane_reports[-1]['status']}; {len(events)} raw PNGs checked",flush=True)
        except Exception as exc:
            r.check("lane_review_exception",False,{"lane":lane,"exception":type(exc).__name__,"message":str(exc)})
            lane_reports.append({"lane":lane,"status":"FAIL","reason":str(exc)})
    r.check("one_editor_project_across_completed_lanes",len(project_names)<=1,sorted(project_names))
    r.check("one_profile_across_completed_lanes",len(all_profiles)<=1,sorted(all_profiles))
    alignment=None
    if set(rows_by_lane)=={1,2,3,4} and not r.failures:
        first=rows_by_lane[1][0]["editor_binding"]
        checked,_=validate_alignment({lane:str(RUN/f"evidence/frame-events-lane{lane}.jsonl") for lane in range(1,5)},tape,allow_mock=False,expected_replay_sha256=replay_sha,expected_render_profile_sha256=first["render_profile_sha256"],expected_output_root=first["normalized_output_root"])
        alignment=checked.to_dict()
        r.check("existing_full_alignment_readonly",checked.ok,checked.problems)
    state_summaries=[]
    candidates=[BASE/"story-after-128-editor-state.json",*args.state_snapshot]
    for path in candidates:
        if not path.exists():continue
        state=r.json(path)
        samples=state.get("samples",[])
        state_summaries.append({"source":r.files[str(path.resolve())],"access":state.get("access"),"observations":[{k:s.get("editor",{}).get(k) for k in ("publisher_pid","heartbeat","editor_active","render_active","edit_menu_active")} for s in samples],"same_recording_process":all(s.get("editor",{}).get("publisher_pid")==pid for s in samples) if samples else None})
    for path,binding in list(r.files.items()):
        if Path(path).suffix.lower()!=".png":r.read(path,binding)
    status="FAIL" if r.failures else ("PASS" if set(rows_by_lane)=={1,2,3,4} else "INCOMPLETE")
    report={"schema":"dm-gta-story-editor-integrity-review/1","created_at":datetime.now(timezone.utc).isoformat(),
            "status":status,"scope":"Read-only actual capture integrity diagnostic; not pixel registration or calibration approval",
            "approval_status":"draft","approval_granted":False,"editor_equivalence_verified":False,"intrinsics_calibrated":False,"metric_scale_approved":False,"delivery_eligible":False,"non_trainable":True,
            "requested_lanes":lanes_to_check,"completed_lanes":sorted(rows_by_lane),"pending_lanes":pending,"all_four_collected":set(rows_by_lane)=={1,2,3,4},
            "check_counts":r.checks,"failures":r.failures,"lanes":lane_reports,"selected_stills":selected,
            "full_alignment_readonly_diagnostic":alignment,"producer_state_snapshots":state_summaries,
            "process_binding_semantics":"The frame session is linked to arm and collect; both hello/probe bind the recording PID, live master SHA and preserved master generation. Controller generation may increase across Editor reentry.",
            "tolerances":{"position_world_units":.0005,"normalized_quaternion_degrees":.005,"fov_control_pixel_focal":.1,"replay_clock_ms":1,"player_observation_ns":20_000_000},
            "limits":["Final K is a sealed FOV control readback. It is not independently calibrated intrinsics.","A matching pose/FOV and intact generation fence do not establish matching scene pixels or Editor equivalence.","Player state is sealed recording-side telemetry, not independent Editor replay entity-origin validation.","Audio, optical registration, visual cursor/HUD cleanliness and dynamic-scene exclusion are outside this integrity report.","Only completed collect results are read; active exports are never contacted or modified.","Every raw PNG was fully read, SHA/size checked and PNG structure verified. Non-PNG inputs were rehashed after review.","No finalize, formal QC writer, pipe, UI, source evidence mutation, or gate grant was invoked."],
            "verified_input_files":list(r.files.values()),"helper":{"path":str(Path(__file__).resolve()),"bytes":Path(__file__).stat().st_size,"sha256":sha(Path(__file__).read_bytes())},
            "validation_source":{"path":str(CAPTURE/"gta_capture/evidence.py"),"sha256":sha((CAPTURE/"gta_capture/evidence.py").read_bytes())}}
    output.parent.mkdir(parents=True,exist_ok=True)
    with output.open("x",encoding="utf8") as handle:json.dump(report,handle,ensure_ascii=False,indent=2,allow_nan=False)
    print(json.dumps({"status":status,"completed_lanes":report["completed_lanes"],"pending_lanes":pending,"failures":r.failures[:8],"report":str(output),"sha256":sha(output.read_bytes())},ensure_ascii=False),flush=True)
    return 1 if r.failures else 0


if __name__=="__main__":raise SystemExit(main())
