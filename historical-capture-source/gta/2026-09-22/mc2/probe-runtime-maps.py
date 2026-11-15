"""Read-only bounded game transport/identity snapshots; never changes a map."""
import argparse
import ctypes
import json
import struct
import time
from pathlib import Path
from gta_capture.jsonio import utc_now

p=argparse.ArgumentParser()
p.add_argument('--out',type=Path,required=True)
p.add_argument('--expected-pid',type=int,required=True)
a=p.parse_args()
if a.out.exists(): raise RuntimeError('existing diagnostics must remain intact')
k=ctypes.WinDLL('kernel32',use_last_error=True)
k.OpenFileMappingW.argtypes=[ctypes.c_uint32,ctypes.c_int,ctypes.c_wchar_p]
k.OpenFileMappingW.restype=ctypes.c_void_p
k.MapViewOfFile.argtypes=[ctypes.c_void_p,ctypes.c_uint32,ctypes.c_uint32,ctypes.c_uint32,ctypes.c_size_t]
k.MapViewOfFile.restype=ctypes.c_void_p
k.UnmapViewOfFile.argtypes=[ctypes.c_void_p]
k.CloseHandle.argtypes=[ctypes.c_void_p]
handles=[]
try:
    for name,size in [('DiogenesGtaEditorIdentity',936),('SimpleCameraFxCapture',688)]:
        handle=k.OpenFileMappingW(4,False,'Local\\'+name)
        if not handle: raise ctypes.WinError(ctypes.get_last_error())
        view=k.MapViewOfFile(handle,4,0,0,size)
        handles.append((handle,view))
        if not view: raise ctypes.WinError(ctypes.get_last_error())
    samples=[]
    for i in range(3):
        identity=handles[0][1]
        for retry in range(100):
            first=struct.unpack('<I',ctypes.string_at(identity+8,4))[0]
            raw=ctypes.string_at(identity,936)
            last=struct.unpack('<I',ctypes.string_at(identity+8,4))[0]
            if first==last and first%2==0: break
        else: raise RuntimeError('identity seqlock did not stabilize')
        fields=dict(zip(['publisher_pid','heartbeat','editor_active','edit_menu_active','start_button_armed','export_config_valid','render_active','planned_frame'],struct.unpack_from('<8I',raw,288)))
        if fields['publisher_pid']!=a.expected_pid: raise RuntimeError('identity belongs to another game process')
        fields['sequence']=first
        fields['project_name']=raw[32:288].split(b'\0')[0].decode('utf-8','replace')
        fx=ctypes.string_at(handles[1][1],688)
        values=dict(zip(['magic','version','request_id','ack_id','status','width','height','sample_count','sample_index','quality'],struct.unpack_from('<10I',fx)))
        values.update(dict(zip(['observer_release_id','capture_ready_id','source_frame'],struct.unpack_from('<3I',fx,560))))
        values['generation']=struct.unpack_from('<I',fx,680)[0]
        values['ready_generation']=struct.unpack_from('<I',fx,684)[0]
        samples.append({'editor':fields,'transport':values})
        if i<2: time.sleep(0.5)
finally:
    for handle,view in handles:
        if view: k.UnmapViewOfFile(view)
        k.CloseHandle(handle)
report={'created_at':utc_now(),'access':'FILE_MAP_READ only','samples':samples}
with a.out.open('x',encoding='utf-8') as f: json.dump(report,f,indent=2)
print(json.dumps(report))
