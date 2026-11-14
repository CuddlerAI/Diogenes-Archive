//! Headless, bounded local experiments. Never launches/injects/records a game.
#[cfg(all(windows, debug_assertions))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use dm_depth_probe::d3d12_backend::local_perf_lab::{
        Options, Workload, mapping, run, working_set,
    };
    use std::{fs::OpenOptions, io::Write, path::PathBuf};

    let mut options = Options::default();
    let mut workload_path = None;
    let mut name = None;
    let mut mapping_sweep = false;
    let mut page_query_compare = false;
    let mut mapping_repeats = 96;
    let mut hot_path_args = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("Missing value for {arg}"))
        };
        match arg.as_str() {
            "--mapping-sweep" => mapping_sweep = true,
            "--page-query-compare" => page_query_compare = true,
            "--mapping-repeats" => mapping_repeats = value()?.parse()?,
            "--warp" => options.warp = true,
            "--heap-mib" => options.heap_mib = value()?.parse()?,
            "--threads" => {
                hot_path_args = true;
                options.threads = value()?
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<_, _>>()?
            }
            "--batches" => {
                hot_path_args = true;
                options.batches = value()?.parse()?;
            }
            "--rounds" => {
                hot_path_args = true;
                options.rounds = value()?.parse()?;
            }
            "--descriptor-copies" => {
                hot_path_args = true;
                options.descriptor_copies = value()?.parse()?;
            }
            "--descriptors-per-copy" => {
                hot_path_args = true;
                options.descriptors_per_copy = value()?.parse()?;
            }
            "--descriptor-lock" => {
                hot_path_args = true;
                options.descriptor_lock = value()?;
            }
            "--descriptor-layout" => {
                hot_path_args = true;
                options.descriptor_layout = value()?;
            }
            "--descriptor-handles" => {
                hot_path_args = true;
                options.descriptor_handles = value()?;
            }
            "--descriptor-owner-trace" => {
                hot_path_args = true;
                options.descriptor_owner_trace = true;
            }
            "--no-descriptor-source-cache" => {
                hot_path_args = true;
                options.disable_source_cache = true;
            }
            "--descriptor-refresh-every" => {
                hot_path_args = true;
                options.descriptor_refresh_every = value()?.parse()?;
            }
            "--budget-seconds" => options.budget_seconds = value()?.parse()?,
            "--workload" => workload_path = Some(PathBuf::from(value()?)),
            "--name" => name = Some(value()?),
            "--help" => {
                println!(
                    "dm-depth-perf-lab --name <unique-label> [--threads 1,4,8] [--heap-mib 192] [--batches 64] [--rounds 4] [--descriptor-copies 2000] [--descriptors-per-copy 8] [--descriptor-lock std|eventual|fair|rw] [--descriptor-layout balanced|source_hot] [--descriptor-handles synthetic|native] [--descriptor-refresh-every 0..10000] [--no-descriptor-source-cache] [--descriptor-owner-trace] [--budget-seconds 90] [--workload offsets.json] [--warp]\nNative descriptors require balanced layout; driver copies are timed separately from bookkeeping.\nSource refresh: 0 disables; N rewrites every N copies (synthetic only, separately timed, included in wall time).\nSource cache toggle requires local-perf-lab-source-cache; disabling retains the same layout/epoch writes for A/B.\nOwner trace requires local-perf-lab-lock-trace; disable source cache, no source refresh, at most 2000 copies, 8 descriptors/copy, 2 rounds and 3 thread counts. Intrusive, not a performance A/B.\nSeparate suite: --mapping-sweep [--mapping-repeats 96] [--heap-mib 192] [--budget-seconds 90] [--warp]; no workload/threads/batches/rounds/descriptor arguments.\nOutput: DataMiner/target/local-perf-lab/<name>.json; create_new, never certification."
                );
                println!(
                    "Separate suite: --page-query-compare [--mapping-repeats 96] [--heap-mib 192] [--budget-seconds 90] [--warp]; no hot-path/workload arguments. Fresh single-page metadata or original VirtualQuery fallback, lab only."
                );
                return Ok(());
            }
            _ => return Err(format!("Unknown argument: {arg}").into()),
        }
    }
    let name = name.ok_or("--name is required")?;
    if name.is_empty()
        || name.len() > 80
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("Name must contain 1-80 ASCII alphanumeric/dash/underscore characters".into());
    }
    if !(1..=256).contains(&options.heap_mib) {
        return Err("heap-mib must be 1..256".into());
    }
    if (mapping_sweep && page_query_compare)
        || ((mapping_sweep || page_query_compare) && (hot_path_args || workload_path.is_some()))
    {
        return Err(
            "mapping-sweep/page-query-compare are separate suites; no hot-path/workload arguments"
                .into(),
        );
    }
    if !mapping_sweep && !page_query_compare && mapping_repeats != 96 {
        return Err("mapping-repeats requires mapping-sweep or page-query-compare".into());
    }
    let workload = match workload_path {
        Some(path) => {
            if std::fs::metadata(&path)?.len() > 4 * 1024 * 1024 {
                return Err("Workload exceeds 4 MiB".into());
            }
            serde_json::from_slice(&std::fs::read(path)?)?
        }
        None => Workload::synthetic(options.heap_mib * 1024 * 1024),
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/local-perf-lab");
    std::fs::create_dir_all(&root)?;
    let path = root.join(format!("{name}.json"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    eprintln!(
        "Local perf lab: bounded CPU workload + private D3D12 upload Map; no game, capture, or FPS claim."
    );
    let result = if page_query_compare {
        working_set::run(&options, mapping_repeats)
            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
    } else if mapping_sweep {
        mapping::run(&options, mapping_repeats)
            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
    } else {
        run(options, workload).and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
    };
    match result {
        Ok(report) => {
            serde_json::to_writer_pretty(&mut file, &report)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            println!("Completed: {}", path.display());
            Ok(())
        }
        Err(error) => {
            serde_json::to_writer_pretty(
                &mut file,
                &serde_json::json!({"schema":"dm-local-depth-perf-lab-failure/v1", "completed":false, "certified":false, "reason":error}),
            )?;
            file.sync_all()?;
            Err(error.into())
        }
    }
}

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!(
        "This local lab requires Windows and debug assertions; unavailable in ordinary release."
    );
    std::process::exit(2);
}
