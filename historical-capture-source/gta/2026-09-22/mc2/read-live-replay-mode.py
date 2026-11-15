import ctypes
import json
import re
import time
from pathlib import Path

log=Path('C:/Program Files (x86)/Steam/steamapps/common/Grand Theft Auto V/RockstarEditorPlus/RockstarEditorPlus.log')
match=re.search(r'g_ReplayMode\s*=\s*([0-9A-Fa-f]+)\s*\(rva (0x[0-9A-Fa-f]+)\)',log.read_text())
if not match: raise RuntimeError('Current producer did not report resolved replay mode')
address=int(match[1],16)
k=ctypes.WinDLL('kernel32',use_last_error=True)
k.OpenProcess.argtypes=[ctypes.c_uint32,ctypes.c_int,ctypes.c_uint32]
k.OpenProcess.restype=ctypes.c_void_p
k.ReadProcessMemory.argtypes=[ctypes.c_void_p,ctypes.c_void_p,ctypes.c_void_p,ctypes.c_size_t,ctypes.POINTER(ctypes.c_size_t)]
k.CloseHandle.argtypes=[ctypes.c_void_p]
h=k.OpenProcess(0x10,False,40816)
if not h: raise ctypes.WinError(ctypes.get_last_error())
values=[]
try:
    for i in range(3):
        value=ctypes.c_int()
        got=ctypes.c_size_t()
        if not k.ReadProcessMemory(h,address,ctypes.byref(value),4,ctypes.byref(got)) or got.value!=4:
            raise ctypes.WinError(ctypes.get_last_error())
        values.append(value.value)
        if i<2: time.sleep(1)
finally: k.CloseHandle(h)
report={'scope':'read-only 4-byte diagnostic from current RE+ resolved address; UI observed Story beach','pid':40816,'log':str(log),'rva':match[2],'replay_modes':values,'approval_status':'draft'}
with Path(__file__).with_name('story-calibration-raw-mode-01.json').open('x',encoding='utf-8') as f: json.dump(report,f,indent=2)
print(json.dumps(report,indent=2))
