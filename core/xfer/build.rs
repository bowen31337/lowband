// Build script — trains per-class zstd dictionaries (Feature 108).
//
// Only runs when the `full` feature is enabled.  Writes three binary
// dictionary files into $OUT_DIR which compress.rs embeds with include_bytes!.
//
// Dictionary size: 4 KB.  Each class is trained from ~50 representative
// samples (~512 bytes each) covering the structural patterns that appear in
// real IT payloads so the compressor can exploit shared prefixes and tokens.

fn main() {
    #[cfg(feature = "full")]
    train_dictionaries();
}

#[cfg(feature = "full")]
fn train_dictionaries() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out = std::path::Path::new(&out_dir);

    const DICT_SIZE: usize = 4096;

    for (samples, name) in [
        (log_samples(), "dict_logs.bin"),
        (registry_samples(), "dict_registry.bin"),
        (config_samples(), "dict_config.bin"),
    ] {
        let dict = zstd::dict::from_samples(&samples, DICT_SIZE)
            .unwrap_or_else(|e| panic!("failed to train {name}: {e}"));
        std::fs::write(out.join(name), &dict)
            .unwrap_or_else(|e| panic!("failed to write {name}: {e}"));
    }
}

// ── Sample generators ────────────────────────────────────────────────────────

#[cfg(feature = "full")]
fn log_samples() -> Vec<Vec<u8>> {
    let levels = ["INFO ", "DEBUG", "WARN ", "ERROR"];
    let modules = [
        "[svc.agent]  ",
        "[xfer.chunk] ",
        "[xfer.fec]   ",
        "[lbtp.pacer] ",
        "[platform]   ",
    ];
    let messages = [
        "Session established peer=192.168.1.{n} session_id={sid}",
        "Chunk deduped hash=4f9a2b{n:02x} chunk_id={cid} bytes=4096",
        "FEC repair symbol sent seq={n} object_id={oid} channel=7",
        "Gear transition {n} -> {m} thermal_pressure=0.{p:02} cpu_load=0.{q:02}",
        "Bulk transfer queued bytes={n}0240 headroom_budget={b} session={sid}",
        "Pacer tick dt=16ms voice_q=0 input_q=0 bulk_q={n}",
        "Chunk cache hit ratio={n}.{m:02}% peer={sid}",
        "RaptorQ ACK received object_id={oid} repair_sent={n}",
        "zstd compress chunk_id={cid} level=3 in={n}0240 out={m}4096",
        "Session teardown peer=192.168.1.{n} duration_ms={d}0",
    ];

    let mut samples = Vec::with_capacity(60);
    let mut idx: usize = 0;
    for _ in 0..6 {
        for level in levels {
            for module in modules {
                let msg = messages[idx % messages.len()]
                    .replace("{n}", &format!("{}", (idx * 7 + 13) % 256))
                    .replace("{m}", &format!("{}", (idx * 3 + 5) % 100))
                    .replace("{p}", &format!("{}", (idx * 11 + 7) % 100))
                    .replace("{q}", &format!("{}", (idx * 17 + 3) % 100))
                    .replace("{b}", &format!("{}", (idx * 23 + 1) % 1000))
                    .replace("{d}", &format!("{}", (idx * 31 + 17) % 10000))
                    .replace("{sid}", &format!("a3f9c2d{:x}", idx % 16))
                    .replace("{cid}", &format!("00{:02x}ff{:02x}", idx % 256, (idx + 1) % 256))
                    .replace("{oid}", &format!("obj{:04x}", idx % 65536));
                let line = format!(
                    "2024-01-15 10:{:02}:{:02}.{:03} {level} {module}{msg}\n",
                    (idx / 60) % 60,
                    idx % 60,
                    (idx * 37) % 1000,
                );
                samples.push(line.into_bytes());
                idx += 1;
            }
        }
    }
    samples
}

#[cfg(feature = "full")]
fn registry_samples() -> Vec<Vec<u8>> {
    let hives = [
        "HKEY_LOCAL_MACHINE\\SOFTWARE\\LowBand",
        "HKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\LowBandSvc",
        "HKEY_LOCAL_MACHINE\\SOFTWARE\\Policies\\LowBand",
        "HKEY_CURRENT_USER\\SOFTWARE\\LowBand",
        "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run",
    ];
    let value_blocks = [
        r#""Version"="1.{n}.{m}"
"InstallPath"="C:\\Program Files\\LowBand"
"ServiceName"="LowBandSvc"
"DataPath"="C:\\ProgramData\\LowBand"
"#,
        r#""Start"=dword:00000002
"Type"=dword:00000010
"ErrorControl"=dword:00000001
"ImagePath"=hex(2):43,00,3a,00,5c,00,50,00,72,00
"#,
        r#""MaxSessions"=dword:0000000{n}
"LogLevel"=dword:00000002
"TelemetryEnabled"=dword:00000000
"UpdatePolicy"="defer"
"#,
        r#""PeerId"="{sid}"
"Tier"="standard"
"CpuCeiling"=dword:00000{n:02x}
"GearPolicy"="auto"
"#,
        r#""LowBand"="C:\\Program Files\\LowBand\\lowband.exe"
"DisplayName"="LowBand Remote Desktop Agent"
"Publisher"="LowBand Ltd"
"UninstallString"="C:\\Program Files\\LowBand\\uninstall.exe"
"#,
    ];

    let mut samples = Vec::with_capacity(55);
    for i in 0..55usize {
        let hive = hives[i % hives.len()];
        let block = value_blocks[i % value_blocks.len()]
            .replace("{n}", &format!("{}", (i * 7 + 3) % 16))
            .replace("{m}", &format!("{}", (i * 11 + 1) % 100))
            .replace("{sid}", &format!("a3f9c2d1-{:04x}-{:04x}-9a2d-5e6f7a8b9c{:02x}", i, i + 1, i % 256));
        let entry = format!(
            "Windows Registry Editor Version 5.00\r\n\r\n[{hive}\\SubKey{i:02}]\r\n{block}\r\n"
        );
        samples.push(entry.into_bytes());
    }
    samples
}

#[cfg(feature = "full")]
fn config_samples() -> Vec<Vec<u8>> {
    let sections = [
        ("[agent]\npeer_id = \"{sid}\"\nlog_level = \"info\"\nmax_sessions = {n}\n\
         [agent.network]\nbind_addr = \"0.0.0.0:{port}\"\nkeepAlive_ms = 5000\n"),
        ("[transfer]\nchunk_size_min = 8192\nchunk_size_max = 65536\n\
         compression_foreground = 3\ncompression_background = 19\n\
         dictionary_class = \"{cls}\"\ndedup_enabled = true\n"),
        ("[platform]\ncpu_ceiling = {n}\ngear_policy = \"auto\"\n\
         thermal_poll_ms = 1000\nvoice_priority_channels = [1, 2]\n\
         input_priority_channels = [3]\nbulk_channels = [7]\n"),
        ("[fec]\nenabled = true\nrepair_symbols = {n}\nchannel = 7\n\
         ack_timeout_ms = 250\nmax_window = 64\nmin_window = 8\n"),
        ("[logging]\nlevel = \"info\"\nfile = \"C:\\\\ProgramData\\\\LowBand\\\\lowband.log\"\n\
         max_size_mb = {n}0\nrotate_count = 5\nformat = \"text\"\n"),
    ];
    let classes = ["logs", "registry", "config"];

    let mut samples = Vec::with_capacity(55);
    for i in 0..55usize {
        let tmpl = sections[i % sections.len()]
            .replace("{n}", &format!("{}", (i * 7 + 20) % 80 + 20))
            .replace("{port}", &format!("{}", 40000 + i % 1000))
            .replace("{cls}", classes[i % classes.len()])
            .replace("{sid}", &format!("a3f9c2d1-{:04x}-7c3f-9a2d-5e6f7a8b9c{:02x}", i, i % 256));
        samples.push(tmpl.into_bytes());
    }
    samples
}
