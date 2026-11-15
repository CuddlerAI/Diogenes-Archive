import ctypes
import json
import struct
import time
from pathlib import Path

k=ctypes.WinDLL('kernel32',use_last_error=True)
k.OpenFileMappingW.argtypes=[ctypes.c_uint32,ctypes.c_int,ctypes.c_wchar_p]
k.OpenFileMappingW.restype=ctypes.c_void_p
k.MapViewOfFile.argtypes=[ctypes.c_void_p,ctypes.c_uint32,ctypes.c_uint32,ctypes.c_uint32,ctypes.c_size_t]
k.MapViewOfFile.restype=ctypes.c_void_p
k.UnmapViewOfFile.argtypes=[ctypes.c_void_p]
k.CloseHandle.argtypes=[ctypes.c_void_p]
h=k.OpenFileMappingW(4,False,'Local\\DiogenesGtaEditorIdentity')
if not h: raise ctypes.WinError(ctypes.get_last_error())
v=k.MapViewOfFile(h,4,0,0,936)
if not v: raise ctypes.WinError(ctypes.get_last_error())
out=[]
try:
    for sample in range(3):
        for retry in range(100):
            a=struct.unpack('<I',ctypes.string_at(v+8,4))[0]
            raw=ctypes.string_at(v,936)
            b=struct.unpack('<I',ctypes.string_at(v+8,4))[0]
            if a==b and a%2==0: break
        else: raise RuntimeError('unstable seqlock')
        row=dict(zip(['magic','version','sequence','capture_request_id','clip_count','clip_index'],struct.unpack_from('<IIIIIi',raw,0)))
        row.update(dict(zip(['publisher_pid','heartbeat','editor_active','edit_menu_active','start_button_armed','export_config_valid','render_active','planned_frame'],struct.unpack_from('<8I',raw,288))))
        row['project_name']=raw[32:288].split(b'\0')[0].decode('utf-8','replace')
        out.append(row)
        if sample<2: time.sleep(1)
finally:
    k.UnmapViewOfFile(v)
    k.CloseHandle(h)
target=Path(__file__).with_name('story-calibration-editor-state-01.json')
with target.open('x',encoding='utf-8') as f: json.dump({'access':'FILE_MAP_READ only','samples':out},f,indent=2)
print(json.dumps(out,indent=2))
