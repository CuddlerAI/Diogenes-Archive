"""Read-only, bounded inspection of the pinned game's cursor-update code."""
import sys
import ctypes
import time
sys.path.insert(0, r'C:\capture\gta\analysis-deps')
import capstone
import pefile

path = r'C:\Program Files (x86)\Steam\steamapps\common\Grand Theft Auto V\GTA5.exe'
pe = pefile.PE(path, fast_load=True)
start = 0x2F6CDC
base = 0x7FF65C550000  # observed in this process's RE+ startup log
pid = 13340  # observed Story Mode PID; never use on another process
kernel = ctypes.WinDLL('kernel32', use_last_error=True)
kernel.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_bool, ctypes.c_ulong]
kernel.OpenProcess.restype = ctypes.c_void_p
kernel.ReadProcessMemory.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.POINTER(ctypes.c_size_t)]
kernel.CloseHandle.argtypes = [ctypes.c_void_p]
handle = kernel.OpenProcess(0x0010, False, pid)  # PROCESS_VM_READ only
if not handle:
    raise ctypes.WinError(ctypes.get_last_error())
def read(address, length):
    result = ctypes.create_string_buffer(length)
    count = ctypes.c_size_t()
    if not kernel.ReadProcessMemory(handle, address, result, length, ctypes.byref(count)) or count.value != length:
        raise ctypes.WinError(ctypes.get_last_error())
    return result.raw
try:
    code = read(base + start, 0x165)
    for i in range(24 if '--watch' in sys.argv else 1):
        print('cursor-state', i,
              list(read(base+0x1F81680, 3)),
              'movie', int.from_bytes(read(base+0x1D417A8, 4), 'little', signed=True),
              'root', int.from_bytes(read(base+0x1D41754, 4), 'little', signed=True),
              'replay-mode', int.from_bytes(read(base+0x1EC264C, 4), 'little'), flush=True)
        if '--watch' in sys.argv:
            time.sleep(1)
finally:
    kernel.CloseHandle(handle)
dis = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
if '--watch' not in sys.argv:
    for inst in dis.disasm(code, base + start):
        print(f'{inst.address-base:08x} {inst.mnemonic:8} {inst.op_str}')
