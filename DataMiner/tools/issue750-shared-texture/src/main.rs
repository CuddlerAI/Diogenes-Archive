use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::mem::size_of;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, INVALID_HANDLE_VALUE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11Device1,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter1, IDXGIDevice,
    IDXGIFactory1, IDXGIResource1,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::{Interface, PCWSTR};

const WIDTH: u32 = 192;
const HEIGHT: u32 = 8;
const BYTES_PER_PIXEL: usize = 4;
const ROW_BYTES: usize = WIDTH as usize * BYTES_PER_PIXEL;
const IMAGE_BYTES: usize = ROW_BYTES * HEIGHT as usize;
const RING_SIZE: usize = 96;
const CONTROL_MAGIC: u64 = 0x4937_3530_5458_5231; // "I750TXR1"
const CONTROL_ABI: u32 = 1;

#[repr(C, align(64))]
struct SharedControl {
    magic: u64,
    abi: u32,
    ring_size: u32,
    width: u32,
    height: u32,
    writer_luid_low: u32,
    writer_luid_high: i32,
    reader_luid_low: AtomicU32,
    reader_luid_high: AtomicU32,
    reader_ready: AtomicU32,
    stop_requested: AtomicU32,
    latest_published: AtomicU64,
    reader_verified: AtomicU64,
    reader_mismatches: AtomicU64,
    reader_overruns: AtomicU64,
    reader_open_failures: AtomicU64,
    slots: [AtomicU64; RING_SIZE],
}

impl SharedControl {
    fn new(adapter: &AdapterIdentity) -> Self {
        Self {
            magic: CONTROL_MAGIC,
            abi: CONTROL_ABI,
            ring_size: RING_SIZE as u32,
            width: WIDTH,
            height: HEIGHT,
            writer_luid_low: adapter.luid_low,
            writer_luid_high: adapter.luid_high,
            reader_luid_low: AtomicU32::new(0),
            reader_luid_high: AtomicU32::new(0),
            reader_ready: AtomicU32::new(0),
            stop_requested: AtomicU32::new(0),
            latest_published: AtomicU64::new(0),
            reader_verified: AtomicU64::new(0),
            reader_mismatches: AtomicU64::new(0),
            reader_overruns: AtomicU64::new(0),
            reader_open_failures: AtomicU64::new(0),
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.magic == CONTROL_MAGIC,
            "control mapping magic mismatch"
        );
        ensure!(self.abi == CONTROL_ABI, "control mapping ABI mismatch");
        ensure!(self.ring_size == RING_SIZE as u32, "ring size mismatch");
        ensure!(
            self.width == WIDTH && self.height == HEIGHT,
            "texture geometry mismatch"
        );
        Ok(())
    }
}

struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    control: NonNull<SharedControl>,
}

impl Mapping {
    fn create(name: &str, adapter: &AdapterIdentity) -> anyhow::Result<Self> {
        let name = wide(name);
        let bytes = u32::try_from(size_of::<SharedControl>())?;
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                bytes,
                PCWSTR(name.as_ptr()),
            )
        }
        .context("CreateFileMappingW failed")?;
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize) };
        if view.Value.is_null() {
            let _ = unsafe { CloseHandle(handle) };
            bail!("MapViewOfFile returned null");
        }
        let control = NonNull::new(view.Value.cast::<SharedControl>())
            .context("control mapping pointer is null")?;
        unsafe { control.as_ptr().write(SharedControl::new(adapter)) };
        Ok(Self {
            handle,
            view,
            control,
        })
    }

    fn open(name: &str) -> anyhow::Result<Self> {
        let name = wide(name);
        let handle =
            unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr())) }
                .context("OpenFileMappingW failed")?;
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            let _ = unsafe { CloseHandle(handle) };
            bail!("MapViewOfFile returned null");
        }
        let control = NonNull::new(view.Value.cast::<SharedControl>())
            .context("control mapping pointer is null")?;
        unsafe { control.as_ref() }.validate()?;
        Ok(Self {
            handle,
            view,
            control,
        })
    }

    fn control(&self) -> &SharedControl {
        unsafe { self.control.as_ref() }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unsafe { UnmapViewOfFile(self.view) };
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[derive(Clone, Debug, Serialize)]
struct AdapterIdentity {
    index: u32,
    description: String,
    vendor_id: u32,
    device_id: u32,
    luid_high: i32,
    luid_low: u32,
    feature_level: u32,
    driver_version: String,
}

struct D3dContext {
    device: ID3D11Device,
    device1: ID3D11Device1,
    context: ID3D11DeviceContext,
    adapter: AdapterIdentity,
}

impl D3dContext {
    fn create(adapter_index: u32) -> anyhow::Result<Self> {
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1 failed")?;
        let adapter = unsafe { factory.EnumAdapters1(adapter_index) }
            .with_context(|| format!("DXGI adapter index {adapter_index} is unavailable"))?;
        let desc = unsafe { adapter.GetDesc1() }.context("IDXGIAdapter1::GetDesc1 failed")?;
        ensure!(
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0,
            "adapter {adapter_index} is software; hardware evidence requires a hardware adapter"
        );
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device = None;
        let mut context = None;
        let mut selected_level = D3D_FEATURE_LEVEL::default();
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut selected_level),
                Some(&mut context),
            )
        }
        .with_context(|| format!("D3D11CreateDevice failed on adapter {adapter_index}"))?;
        let device = device.context("D3D11 returned no device")?;
        let device1 = device.cast().context("ID3D11Device1 is unavailable")?;
        let context = context.context("D3D11 returned no immediate context")?;
        let driver_version = adapter_driver_version(&adapter)?;
        Ok(Self {
            device,
            device1,
            context,
            adapter: AdapterIdentity {
                index: adapter_index,
                description: utf16_z(&desc.Description),
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                luid_high: desc.AdapterLuid.HighPart,
                luid_low: desc.AdapterLuid.LowPart,
                feature_level: selected_level.0 as u32,
                driver_version,
            },
        })
    }
}

struct SharedRing {
    textures: Vec<ID3D11Texture2D>,
    handles: Vec<HANDLE>,
}

impl SharedRing {
    fn create(device: &ID3D11Device, session: &str) -> anyhow::Result<Self> {
        let mut textures = Vec::with_capacity(RING_SIZE);
        let mut handles = Vec::with_capacity(RING_SIZE);
        for slot in 0..RING_SIZE {
            let texture = create_texture(
                device,
                D3D11_USAGE_DEFAULT,
                0,
                (D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE).0 as u32,
            )
            .with_context(|| format!("create shared texture slot {slot}"))?;
            let resource: IDXGIResource1 = texture
                .cast()
                .with_context(|| format!("slot {slot} does not expose IDXGIResource1"))?;
            let name = wide(&texture_name(session, slot));
            let handle = unsafe {
                resource.CreateSharedHandle(
                    None,
                    (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
                    PCWSTR(name.as_ptr()),
                )
            }
            .with_context(|| format!("CreateSharedHandle failed for slot {slot}"))?;
            textures.push(texture);
            handles.push(handle);
        }
        Ok(Self { textures, handles })
    }

    fn open(device: &ID3D11Device1, session: &str) -> anyhow::Result<Vec<ID3D11Texture2D>> {
        (0..RING_SIZE)
            .map(|slot| {
                let name = wide(&texture_name(session, slot));
                unsafe {
                    device.OpenSharedResourceByName::<_, ID3D11Texture2D>(
                        PCWSTR(name.as_ptr()),
                        DXGI_SHARED_RESOURCE_READ.0,
                    )
                }
                .with_context(|| format!("OpenSharedResourceByName failed for slot {slot}"))
            })
            .collect()
    }
}

impl Drop for SharedRing {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            let _ = unsafe { CloseHandle(handle) };
        }
    }
}

fn create_texture(
    device: &ID3D11Device,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    cpu_access: u32,
    misc: u32,
) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: if usage == D3D11_USAGE_DEFAULT {
            D3D11_BIND_SHADER_RESOURCE.0 as u32
        } else {
            0
        },
        CPUAccessFlags: cpu_access,
        MiscFlags: misc,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("ID3D11Device::CreateTexture2D failed")?;
    texture.context("CreateTexture2D returned null")
}

#[derive(Clone, Debug)]
struct RunArgs {
    writer_adapter: u32,
    reader_adapter: u32,
    hz: u32,
    warmup_seconds: u64,
    alive_seconds: u64,
    dead_seconds: u64,
    reader_lag_slots: u64,
    output: PathBuf,
}

impl Default for RunArgs {
    fn default() -> Self {
        Self {
            writer_adapter: 0,
            reader_adapter: 0,
            hz: 240,
            warmup_seconds: 2,
            alive_seconds: 10,
            dead_seconds: 10,
            reader_lag_slots: 4,
            output: PathBuf::from("issue750-evidence"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Percentiles {
    count: usize,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    max_us: f64,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    schema: &'static str,
    session: String,
    ring_size: usize,
    width: u32,
    height: u32,
    target_hz: u32,
    writer_adapter: AdapterIdentity,
    reader_adapter: AdapterIdentity,
    same_adapter: bool,
    warmup_seconds: u64,
    reader_alive_seconds: u64,
    reader_dead_seconds: u64,
    reader_lag_slots: u64,
    copy_call_reader_alive: Percentiles,
    copy_call_reader_dead: Percentiles,
    copy_plus_flush_reader_alive: Percentiles,
    copy_plus_flush_reader_dead: Percentiles,
    writer_samples: usize,
    reader_verified_before_kill: u64,
    reader_mismatches_before_kill: u64,
    reader_overruns_before_kill: u64,
    reader_open_failures: u64,
    reader_kill_succeeded: bool,
    notes: Vec<&'static str>,
}

#[derive(Debug)]
struct Sample {
    ordinal: u64,
    slot: usize,
    phase: &'static str,
    copy_ticks: i64,
    total_ticks: i64,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("adapters") => list_adapters(),
        Some("run") => run_writer(parse_run_args(&args[2..])?),
        Some("reader") => run_reader(&args[2..]),
        _ => {
            print_usage();
            bail!("expected one of: adapters, run, reader")
        }
    }
}

fn print_usage() {
    eprintln!(
        "Usage:\n  issue750-shared-texture adapters\n  issue750-shared-texture run [--writer-adapter N] [--reader-adapter N] [--hz 240] [--warmup-seconds 2] [--alive-seconds 10] [--dead-seconds 10] [--reader-lag-slots 4] [--output PATH]"
    );
}

fn parse_run_args(raw: &[String]) -> anyhow::Result<RunArgs> {
    let mut args = RunArgs::default();
    let mut index = 0;
    while index < raw.len() {
        let key = &raw[index];
        let value = raw
            .get(index + 1)
            .with_context(|| format!("{key} requires a value"))?;
        match key.as_str() {
            "--writer-adapter" => args.writer_adapter = value.parse()?,
            "--reader-adapter" => args.reader_adapter = value.parse()?,
            "--hz" => args.hz = value.parse()?,
            "--warmup-seconds" => args.warmup_seconds = value.parse()?,
            "--alive-seconds" => args.alive_seconds = value.parse()?,
            "--dead-seconds" => args.dead_seconds = value.parse()?,
            "--reader-lag-slots" => args.reader_lag_slots = value.parse()?,
            "--output" => args.output = PathBuf::from(value),
            _ => bail!("unknown option: {key}"),
        }
        index += 2;
    }
    ensure!(args.hz > 0 && args.hz <= 10_000, "--hz must be 1..=10000");
    ensure!(args.alive_seconds > 0, "--alive-seconds must be positive");
    ensure!(args.dead_seconds > 0, "--dead-seconds must be positive");
    ensure!(
        args.reader_lag_slots < RING_SIZE as u64,
        "--reader-lag-slots must be smaller than the ring"
    );
    Ok(args)
}

fn run_writer(args: RunArgs) -> anyhow::Result<()> {
    fs::create_dir_all(&args.output)
        .with_context(|| format!("create output directory {}", args.output.display()))?;
    let writer = D3dContext::create(args.writer_adapter)?;
    let reader_identity = D3dContext::create(args.reader_adapter)?.adapter;
    let clock = QpcClock::new()?;
    let session = format!("{}-{}", std::process::id(), clock.now()?);
    let mapping_name = control_name(&session);
    let mapping = Mapping::create(&mapping_name, &writer.adapter)?;
    let control = mapping.control();
    let ring = SharedRing::create(&writer.device, &session)?;
    let source = create_texture(&writer.device, D3D11_USAGE_DEFAULT, 0, 0)?;

    let mut reader = spawn_reader(&session, args.reader_adapter, args.reader_lag_slots)?;
    wait_for_reader(control, &mut reader, Duration::from_secs(15))?;

    let raw_path = args.output.join("writer-samples.csv");
    let mut raw = BufWriter::new(
        File::create(&raw_path).with_context(|| format!("create {}", raw_path.display()))?,
    );
    writeln!(
        raw,
        "ordinal,slot,phase,copy_ticks,total_ticks,copy_us,total_us"
    )?;

    let period = Duration::from_secs_f64(1.0 / f64::from(args.hz));
    let warmup_end = Instant::now() + Duration::from_secs(args.warmup_seconds);
    let alive_end = warmup_end + Duration::from_secs(args.alive_seconds);
    let dead_end = alive_end + Duration::from_secs(args.dead_seconds);
    let mut next_deadline = Instant::now();
    let mut ordinal = 0_u64;
    let mut samples = Vec::new();
    let mut pixels = vec![0_u8; IMAGE_BYTES];
    let mut killed = false;
    let mut reader_verified_before_kill = 0;
    let mut reader_mismatches_before_kill = 0;
    let mut reader_overruns_before_kill = 0;
    let mut reader_kill_succeeded = false;

    while Instant::now() < dead_end {
        pace_until(next_deadline);
        next_deadline += period;
        ordinal += 1;
        let slot = ordinal as usize % RING_SIZE;
        fill_pattern(&mut pixels, ordinal, slot);
        unsafe {
            writer.context.UpdateSubresource(
                &source,
                0,
                None,
                pixels.as_ptr().cast::<c_void>(),
                ROW_BYTES as u32,
                IMAGE_BYTES as u32,
            );
        }
        let before = clock.now()?;
        unsafe {
            writer.context.CopySubresourceRegion(
                &ring.textures[slot],
                0,
                0,
                0,
                0,
                &source,
                0,
                None,
            );
        }
        let after_copy = clock.now()?;
        unsafe { writer.context.Flush() };
        let after_flush = clock.now()?;
        control.slots[slot].store(ordinal, Ordering::Release);
        control.latest_published.store(ordinal, Ordering::Release);

        let now = Instant::now();
        let phase = if now < warmup_end {
            "warmup"
        } else if now < alive_end {
            "reader_alive"
        } else {
            if !killed {
                reader_verified_before_kill = control.reader_verified.load(Ordering::Acquire);
                reader_mismatches_before_kill = control.reader_mismatches.load(Ordering::Acquire);
                reader_overruns_before_kill = control.reader_overruns.load(Ordering::Acquire);
                reader_kill_succeeded = reader.kill().is_ok();
                let _ = reader.wait();
                killed = true;
            }
            "reader_dead"
        };
        let sample = Sample {
            ordinal,
            slot,
            phase,
            copy_ticks: after_copy - before,
            total_ticks: after_flush - before,
        };
        writeln!(
            raw,
            "{},{},{},{},{},{:.6},{:.6}",
            sample.ordinal,
            sample.slot,
            sample.phase,
            sample.copy_ticks,
            sample.total_ticks,
            clock.to_us(sample.copy_ticks),
            clock.to_us(sample.total_ticks),
        )?;
        if phase != "warmup" {
            samples.push(sample);
        }
    }
    control.stop_requested.store(1, Ordering::Release);
    raw.flush()?;

    let alive_copy = sample_values(&samples, "reader_alive", |sample| sample.copy_ticks);
    let dead_copy = sample_values(&samples, "reader_dead", |sample| sample.copy_ticks);
    let alive_total = sample_values(&samples, "reader_alive", |sample| sample.total_ticks);
    let dead_total = sample_values(&samples, "reader_dead", |sample| sample.total_ticks);
    ensure!(
        !alive_copy.is_empty(),
        "no reader_alive samples were recorded"
    );
    ensure!(
        !dead_copy.is_empty(),
        "no reader_dead samples were recorded"
    );

    let summary = RunSummary {
        schema: "diogenes.issue750.shared-texture/v1",
        session: session.clone(),
        ring_size: RING_SIZE,
        width: WIDTH,
        height: HEIGHT,
        target_hz: args.hz,
        same_adapter: writer.adapter.luid_high == reader_identity.luid_high
            && writer.adapter.luid_low == reader_identity.luid_low,
        writer_adapter: writer.adapter,
        reader_adapter: reader_identity,
        warmup_seconds: args.warmup_seconds,
        reader_alive_seconds: args.alive_seconds,
        reader_dead_seconds: args.dead_seconds,
        reader_lag_slots: args.reader_lag_slots,
        copy_call_reader_alive: percentiles(alive_copy, &clock),
        copy_call_reader_dead: percentiles(dead_copy, &clock),
        copy_plus_flush_reader_alive: percentiles(alive_total, &clock),
        copy_plus_flush_reader_dead: percentiles(dead_total, &clock),
        writer_samples: samples.len(),
        reader_verified_before_kill,
        reader_mismatches_before_kill,
        reader_overruns_before_kill,
        reader_open_failures: control.reader_open_failures.load(Ordering::Acquire),
        reader_kill_succeeded,
        notes: vec![
            "CopySubresourceRegion and Flush are timed separately; Microsoft documents Flush as asynchronous.",
            "The reader is force-terminated between phases; writer_dead samples are collected after wait() confirms exit.",
            "No keyed mutex is created or acquired anywhere in this experiment.",
        ],
    };
    let summary_path = args.output.join("summary.json");
    serde_json::to_writer_pretty(BufWriter::new(File::create(&summary_path)?), &summary)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn spawn_reader(session: &str, adapter: u32, lag_slots: u64) -> anyhow::Result<Child> {
    Command::new(std::env::current_exe()?)
        .arg("reader")
        .arg("--session")
        .arg(session)
        .arg("--adapter")
        .arg(adapter.to_string())
        .arg("--lag-slots")
        .arg(lag_slots.to_string())
        .spawn()
        .context("spawn reader process")
}

fn wait_for_reader(
    control: &SharedControl,
    reader: &mut Child,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if control.reader_ready.load(Ordering::Acquire) == 1 {
            return Ok(());
        }
        if let Some(status) = reader.try_wait()? {
            bail!("reader exited before ready: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = reader.kill();
    let _ = reader.wait();
    bail!("reader did not become ready within {timeout:?}")
}

fn run_reader(raw: &[String]) -> anyhow::Result<()> {
    let session = option(raw, "--session")?.to_owned();
    let adapter_index: u32 = option(raw, "--adapter")?.parse()?;
    let lag_slots: u64 = option(raw, "--lag-slots")?.parse()?;
    ensure!(lag_slots < RING_SIZE as u64, "reader lag exceeds ring");
    let mapping = Mapping::open(&control_name(&session))?;
    let control = mapping.control();
    let reader = D3dContext::create(adapter_index)?;
    control
        .reader_luid_low
        .store(reader.adapter.luid_low, Ordering::Release);
    control
        .reader_luid_high
        .store(reader.adapter.luid_high as u32, Ordering::Release);
    let textures = match SharedRing::open(&reader.device1, &session) {
        Ok(textures) => textures,
        Err(error) => {
            control.reader_open_failures.fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }
    };
    let staging: Vec<ID3D11Texture2D> = (0..RING_SIZE)
        .map(|_| {
            create_texture(
                &reader.device,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ.0 as u32,
                0,
            )
        })
        .collect::<anyhow::Result<_>>()?;
    control.reader_ready.store(1, Ordering::Release);

    let mut next = 1_u64;
    while control.stop_requested.load(Ordering::Acquire) == 0 {
        let latest = control.latest_published.load(Ordering::Acquire);
        if latest < next.saturating_add(lag_slots) {
            thread::yield_now();
            continue;
        }
        if latest.saturating_sub(next) >= RING_SIZE as u64 {
            let skipped = latest - next - RING_SIZE as u64 + 1;
            control
                .reader_overruns
                .fetch_add(skipped, Ordering::Relaxed);
            next = latest - RING_SIZE as u64 + 1;
        }
        let slot = next as usize % RING_SIZE;
        let committed = control.slots[slot].load(Ordering::Acquire);
        if committed != next {
            thread::yield_now();
            continue;
        }
        unsafe { reader.context.CopyResource(&staging[slot], &textures[slot]) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            reader
                .context
                .Map(&staging[slot], 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .with_context(|| format!("Map staging slot {slot}"))?;
        let valid = unsafe { verify_mapped(mapped, next, slot) };
        unsafe { reader.context.Unmap(&staging[slot], 0) };
        if valid {
            control.reader_verified.fetch_add(1, Ordering::Relaxed);
        } else {
            control.reader_mismatches.fetch_add(1, Ordering::Relaxed);
        }
        next += 1;
    }
    Ok(())
}

unsafe fn verify_mapped(mapped: D3D11_MAPPED_SUBRESOURCE, ordinal: u64, slot: usize) -> bool {
    if mapped.pData.is_null() || mapped.RowPitch < ROW_BYTES as u32 {
        return false;
    }
    for y in 0..HEIGHT as usize {
        let row = unsafe { (mapped.pData as *const u8).add(y * mapped.RowPitch as usize) };
        for x in 0..WIDTH as usize {
            let actual = unsafe { std::slice::from_raw_parts(row.add(x * BYTES_PER_PIXEL), 4) };
            if actual != expected_pixel(ordinal, slot, x, y) {
                return false;
            }
        }
    }
    true
}

fn fill_pattern(output: &mut [u8], ordinal: u64, slot: usize) {
    for y in 0..HEIGHT as usize {
        for x in 0..WIDTH as usize {
            let offset = (y * WIDTH as usize + x) * BYTES_PER_PIXEL;
            output[offset..offset + 4].copy_from_slice(&expected_pixel(ordinal, slot, x, y));
        }
    }
}

fn expected_pixel(ordinal: u64, slot: usize, x: usize, y: usize) -> [u8; 4] {
    let mixed = ordinal
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(((x + y * WIDTH as usize) & 63) as u32);
    [
        (mixed as u8) ^ x as u8,
        ((mixed >> 8) as u8) ^ y as u8,
        ((ordinal >> 16) as u8) ^ slot as u8,
        ((ordinal >> 24) as u8) ^ 0xA5,
    ]
}

fn option<'a>(raw: &'a [String], name: &str) -> anyhow::Result<&'a str> {
    let index = raw
        .iter()
        .position(|value| value == name)
        .with_context(|| format!("missing {name}"))?;
    raw.get(index + 1)
        .map(String::as_str)
        .with_context(|| format!("{name} requires a value"))
}

fn pace_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline - now;
        if remaining > Duration::from_millis(1) {
            thread::sleep(remaining - Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn sample_values(samples: &[Sample], phase: &str, select: impl Fn(&Sample) -> i64) -> Vec<i64> {
    samples
        .iter()
        .filter(|sample| sample.phase == phase)
        .map(select)
        .collect()
}

fn percentiles(mut values: Vec<i64>, clock: &QpcClock) -> Percentiles {
    values.sort_unstable();
    Percentiles {
        count: values.len(),
        p50_us: clock.to_us(quantile(&values, 0.50)),
        p95_us: clock.to_us(quantile(&values, 0.95)),
        p99_us: clock.to_us(quantile(&values, 0.99)),
        max_us: clock.to_us(*values.last().unwrap_or(&0)),
    }
}

fn quantile(values: &[i64], q: f64) -> i64 {
    if values.is_empty() {
        return 0;
    }
    let index = ((values.len() - 1) as f64 * q).ceil() as usize;
    values[index]
}

struct QpcClock {
    frequency: i64,
}

impl QpcClock {
    fn new() -> anyhow::Result<Self> {
        let mut frequency = 0_i64;
        unsafe { QueryPerformanceFrequency(&mut frequency) }
            .context("QueryPerformanceFrequency failed")?;
        ensure!(frequency > 0, "QPC frequency is not positive");
        Ok(Self { frequency })
    }

    fn now(&self) -> anyhow::Result<i64> {
        let mut value = 0_i64;
        unsafe { QueryPerformanceCounter(&mut value) }.context("QueryPerformanceCounter failed")?;
        Ok(value)
    }

    fn to_us(&self, ticks: i64) -> f64 {
        ticks as f64 * 1_000_000.0 / self.frequency as f64
    }
}

fn list_adapters() -> anyhow::Result<()> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut adapters = Vec::new();
    let mut index = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error.into()),
        };
        let desc = unsafe { adapter.GetDesc1() }?;
        adapters.push(serde_json::json!({
            "index": index,
            "description": utf16_z(&desc.Description),
            "vendor_id": desc.VendorId,
            "device_id": desc.DeviceId,
            "luid_high": desc.AdapterLuid.HighPart,
            "luid_low": desc.AdapterLuid.LowPart,
            "software": desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        }));
        index += 1;
    }
    println!("{}", serde_json::to_string_pretty(&adapters)?);
    Ok(())
}

fn adapter_driver_version(adapter: &IDXGIAdapter1) -> anyhow::Result<String> {
    let raw = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
        .context("IDXGIAdapter::CheckInterfaceSupport failed")? as u64;
    let high = (raw >> 32) as u32;
    let low = raw as u32;
    Ok(format!(
        "{}.{}.{}.{}",
        high >> 16,
        high & 0xffff,
        low >> 16,
        low & 0xffff
    ))
}

fn control_name(session: &str) -> String {
    format!("Local\\DiogenesIssue750Control-{session}")
}

fn texture_name(session: &str, slot: usize) -> String {
    format!("Local\\DiogenesIssue750Texture-{session}-{slot:02}")
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn utf16_z(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_pattern_changes_with_ordinal_and_slot() {
        assert_ne!(expected_pixel(1, 1, 0, 0), expected_pixel(2, 2, 0, 0));
        assert_ne!(expected_pixel(1, 1, 0, 0), expected_pixel(1, 1, 1, 0));
    }

    #[test]
    fn quantiles_use_observed_upper_rank() {
        let values = [1, 2, 3, 4, 5];
        assert_eq!(quantile(&values, 0.50), 3);
        assert_eq!(quantile(&values, 0.95), 5);
        assert_eq!(quantile(&values, 0.99), 5);
    }

    #[test]
    fn shared_control_has_fixed_ring_contract() {
        let adapter = AdapterIdentity {
            index: 0,
            description: String::new(),
            vendor_id: 0,
            device_id: 0,
            luid_high: 0,
            luid_low: 0,
            feature_level: 0,
            driver_version: String::new(),
        };
        let control = SharedControl::new(&adapter);
        control.validate().unwrap();
        assert_eq!(control.slots.len(), 96);
    }
}
