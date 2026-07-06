#!/usr/bin/env python3
"""
Generate pre-trained zstd dictionaries for lowband-xfer Feature 108.
Replicates the sample generators from build.rs and writes dict_{logs,registry,config}.bin
to core/xfer/src/dicts/ so they can be embedded via include_bytes! without a build script.
"""
import ctypes, os, pathlib

DICT_SIZE = 4096
OUT = pathlib.Path("core/xfer/src/dicts")
OUT.mkdir(exist_ok=True)

lib = ctypes.CDLL("libzstd.so.1")
lib.ZDICT_trainFromBuffer.restype = ctypes.c_size_t
lib.ZDICT_trainFromBuffer.argtypes = [
    ctypes.c_char_p, ctypes.c_size_t,
    ctypes.c_char_p, ctypes.POINTER(ctypes.c_size_t), ctypes.c_uint,
]
lib.ZDICT_isError.restype = ctypes.c_uint
lib.ZDICT_isError.argtypes = [ctypes.c_size_t]

def train(samples):
    buf = b"".join(samples)
    sizes = (ctypes.c_size_t * len(samples))(*[len(s) for s in samples])
    out = ctypes.create_string_buffer(DICT_SIZE)
    r = lib.ZDICT_trainFromBuffer(out, DICT_SIZE, buf, sizes, len(samples))
    if lib.ZDICT_isError(r):
        raise RuntimeError(f"dict training failed: code {r}")
    return bytes(out.raw[:r])

# ── Log samples ──────────────────────────────────────────────────────────────
def log_samples():
    levels  = ["INFO ", "DEBUG", "WARN ", "ERROR"]
    modules = ["[svc.agent]  ", "[xfer.chunk] ", "[xfer.fec]   ",
               "[lbtp.pacer] ", "[platform]   "]
    msgs    = [
        "Session established peer=192.168.1.{n} session_id={sid}",
        "Chunk deduped hash=4f9a2b{n} chunk_id={cid} bytes=4096",
        "FEC repair symbol sent seq={n} object_id={oid} channel=7",
        "Gear transition {n} -> {m} thermal_pressure=0.{p} cpu_load=0.{q}",
        "Bulk transfer queued bytes={n}0240 headroom_budget={b} session={sid}",
        "Pacer tick dt=16ms voice_q=0 input_q=0 bulk_q={n}",
        "Chunk cache hit ratio={n}.{m}% peer={sid}",
        "RaptorQ ACK received object_id={oid} repair_sent={n}",
        "zstd compress chunk_id={cid} level=3 in={n}0240 out={m}4096",
        "Session teardown peer=192.168.1.{n} duration_ms={d}0",
    ]
    samples, idx = [], 0
    for _ in range(6):
        for lv in levels:
            for mod in modules:
                n   = (idx*7+13)%256;  m = (idx*3+5)%100
                p   = (idx*11+7)%100;  q = (idx*17+3)%100
                b   = (idx*23+1)%1000; d = (idx*31+17)%10000
                sid = f"a3f9c2d{idx%16:x}"
                cid = f"00{idx%256:02x}ff{(idx+1)%256:02x}"
                oid = f"obj{idx%65536:04x}"
                msg = msgs[idx % len(msgs)]
                for k,v in [("{n}",str(n)),("{m}",str(m)),("{p}",str(p)),
                             ("{q}",str(q)),("{b}",str(b)),("{d}",str(d)),
                             ("{sid}",sid),("{cid}",cid),("{oid}",oid)]:
                    msg = msg.replace(k, v)
                line = (f"2024-01-15 10:{(idx//60)%60:02}:{idx%60:02}."
                        f"{(idx*37)%1000:03} {lv} {mod}{msg}\n")
                samples.append(line.encode()); idx += 1
    return samples

# ── Registry samples ─────────────────────────────────────────────────────────
def registry_samples():
    hives = [
        r"HKEY_LOCAL_MACHINE\SOFTWARE\LowBand",
        r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\LowBandSvc",
        r"HKEY_LOCAL_MACHINE\SOFTWARE\Policies\LowBand",
        r"HKEY_CURRENT_USER\SOFTWARE\LowBand",
        r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
    ]
    blocks = [
        '"Version"="1.{n}.{m}"\r\n"InstallPath"="C:\\\\Program Files\\\\LowBand"\r\n',
        '"Start"=dword:00000002\r\n"Type"=dword:00000010\r\n"ErrorControl"=dword:00000001\r\n',
        '"MaxSessions"=dword:0000000{n}\r\n"LogLevel"=dword:00000002\r\n"TelemetryEnabled"=dword:00000000\r\n',
        '"PeerId"="{sid}"\r\n"Tier"="standard"\r\n"CpuCeiling"=dword:0000{n:02x}\r\n',
        '"LowBand"="C:\\\\Program Files\\\\LowBand\\\\lowband.exe"\r\n"Publisher"="LowBand Ltd"\r\n',
    ]
    samples = []
    for i in range(55):
        hive  = hives[i % len(hives)]
        n     = (i*7+3)%16;  m = (i*11+1)%100
        sid   = f"a3f9c2d1-{i:04x}-{i+1:04x}-9a2d-5e6f7a8b9c{i%256:02x}"
        block = blocks[i % len(blocks)]
        for k,v in [("{n}",str(n)),("{m}",str(m)),("{sid}",sid)]:
            block = block.replace(k, v)
        entry = (f"Windows Registry Editor Version 5.00\r\n\r\n"
                 f"[{hive}\\SubKey{i:02}]\r\n{block}\r\n")
        samples.append(entry.encode())
    return samples

# ── Config samples ───────────────────────────────────────────────────────────
def config_samples():
    secs = [
        '[agent]\npeer_id = "{sid}"\nlog_level = "info"\nmax_sessions = {n}\n',
        '[transfer]\nchunk_size_min = 8192\nchunk_size_max = 65536\ncompression_foreground = 3\ncompression_background = 19\ndictionary_class = "{cls}"\ndedup_enabled = true\n',
        '[platform]\ncpu_ceiling = {n}\ngear_policy = "auto"\nthermal_poll_ms = 1000\nvoice_priority_channels = [1, 2]\nbulk_channels = [7]\n',
        '[fec]\nenabled = true\nrepair_symbols = {n}\nchannel = 7\nack_timeout_ms = 250\nmax_window = 64\n',
        '[logging]\nlevel = "info"\nfile = "C:\\\\ProgramData\\\\LowBand\\\\lowband.log"\nmax_size_mb = {n}0\nrotate_count = 5\n',
    ]
    classes = ["logs", "registry", "config"]
    samples = []
    for i in range(55):
        n   = (i*7+20)%80+20;  port = 40000+i%1000
        cls = classes[i % 3]
        sid = f"a3f9c2d1-{i:04x}-7c3f-9a2d-5e6f7a8b9c{i%256:02x}"
        s   = secs[i % len(secs)]
        for k,v in [("{n}",str(n)),("{port}",str(port)),("{cls}",cls),("{sid}",sid)]:
            s = s.replace(k, v)
        samples.append(s.encode())
    return samples

for gen, name in [(log_samples, "dict_logs"), (registry_samples, "dict_registry"), (config_samples, "dict_config")]:
    data = train(gen())
    path = OUT / f"{name}.bin"
    path.write_bytes(data)
    print(f"wrote {path} ({len(data)} bytes)")
