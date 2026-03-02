// src/main.rs
// altered by Apiro Poindexter AI: added --save-interval argument to replace hardcoded 60-tick save period
// altered by Apiro Poindexter AI: auto-detect card size, erase block size, and filesystem fullness for WAF multiplier
// altered by Apiro Poindexter AI: added human-readable descriptor fields to persisted state JSON

use anyhow::{Context, Result};
use clap::Parser;
use nix::sys::statvfs::statvfs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(version, about = "SD card flash wear estimator")]
struct Args {
    /// Block device name (e.g. mmcblk0)
    #[arg(short, long, default_value = "mmcblk0")]
    device: String,

    /// Override flash erase block size in KB. If not set, auto-detected from
    /// /sys/block/<dev>/queue/discard_granularity or optimal_io_size.
    #[arg(short, long)]
    erase_block_kb: Option<u64>,

    /// Rated P/E cycles (TLC≈1000-3000, MLC≈3000-10000)
    #[arg(short, long, default_value_t = 3000)]
    pe_cycles: u64,

    /// Poll interval in seconds
    #[arg(short, long, default_value_t = 5)]
    interval: u64,

    /// State file path for persistence across reboots
    #[arg(long, default_value = "/var/lib/sdwear/state.json")]
    state_file: PathBuf,

    /// How often to save state to disk in seconds (default: 300 = 5 minutes)
    #[arg(long, default_value_t = 300)]
    save_interval: u64,
}

// ─── Persistent state: survives reboots ───
//
// This struct is serialised to JSON. It is designed to be human-readable
// so that an operator can open the file and immediately understand the
// health of the SD card without needing to know the program arguments
// that were used to start the daemon.
//
// Fields are grouped into three sections:
//   1. Descriptor fields  — what device/card this file relates to
//   2. Cumulative metrics — the wear tracking numbers
//   3. Internal counters  — used by the algorithm for delta/reboot detection

#[derive(Serialize, Deserialize, Debug, Clone)]
struct State {

    // ── Section 1: Descriptor fields ──
    // These are refreshed on every save so the file is always self-describing.

    /// The block device being monitored (e.g. "mmcblk0")
    device: String,

    /// Card capacity in GB as reported by the kernel (human readable)
    card_size_gb: f64,

    /// Card capacity in exact bytes as reported by the kernel
    card_size_bytes: u64,

    /// Erase block size in KB (auto-detected or user override)
    erase_block_kb: u64,

    /// Rated P/E cycle endurance used for life% calculation
    pe_cycles_rated: u64,

    /// Mount points detected for this device's partitions at last save
    mount_points: Vec<String>,

    /// Filesystem fullness percentage at last save (worst partition, drives WAF multiplier)
    filesystem_fullness_pct: f64,

    // ── Section 2: Cumulative wear metrics ──

    /// Cumulative 512-byte sectors written as seen from host side
    total_host_sectors_written: u64,

    /// Cumulative write IO count from host side
    total_host_write_ios: u64,

    /// Cumulative estimated bytes actually written to flash (after WAF adjustment)
    estimated_flash_bytes_written: u64,

    /// ★ THE KEY NUMBER ★
    /// Estimated average P/E cycle count per erase block across the whole card.
    /// Calculated as: estimated_flash_bytes_written / card_size_bytes
    /// When this approaches pe_cycles_rated the card is near end of life.
    estimated_avg_pe_cycles: f64,

    /// Estimated remaining life as a percentage of rated P/E endurance.
    /// 100.0 = brand new, 0.0 = end of rated life
    estimated_life_remaining_pct: f64,

    // ── Section 3: Internal algorithm counters ──

    /// Raw kernel write sector counter at last poll — used to compute deltas
    /// and detect reboots (kernel counters reset to 0 on boot)
    last_kernel_write_sectors: u64,

    /// Raw kernel write IO counter at last poll
    last_kernel_write_ios: u64,

    /// Unix timestamp when monitoring first started on this card
    first_started: String,

    /// Unix timestamp of the last state file save
    last_updated: String,

    /// Number of reboots detected since monitoring began
    reboot_count: u64,
}

impl State {
    /// Create a fresh state for a card that has never been monitored before.
    /// Descriptor fields are populated separately after detection in main().
    fn new() -> Self {
        let now = now_string();
        Self {
            // Descriptor fields — filled in after detection, zeroed here
            device: String::new(),
            card_size_gb: 0.0,
            card_size_bytes: 0,
            erase_block_kb: 0,
            pe_cycles_rated: 0,
            mount_points: vec![],
            filesystem_fullness_pct: 0.0,

            // Cumulative metrics
            total_host_sectors_written: 0,
            total_host_write_ios: 0,
            estimated_flash_bytes_written: 0,
            estimated_avg_pe_cycles: 0.0,
            estimated_life_remaining_pct: 100.0,

            // Internal counters
            last_kernel_write_sectors: 0,
            last_kernel_write_ios: 0,
            first_started: now.clone(),
            last_updated: now,
            reboot_count: 0,
        }
    }
}

// ─── Reading /sys/block/<dev>/stat ───

struct KernelStats {
    write_ios: u64,
    write_sectors: u64,
}

fn read_kernel_stats(device: &str) -> Result<KernelStats> {
    let path = format!("/sys/block/{}/stat", device);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Cannot read {path}"))?;

    let fields: Vec<u64> = content
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();

    // Field indices per kernel docs:
    //   0=read_ios 1=read_merges 2=read_sectors 3=read_ticks
    //   4=write_ios 5=write_merges 6=write_sectors 7=write_ticks ...
    Ok(KernelStats {
        write_ios: *fields.get(4).unwrap_or(&0),
        write_sectors: *fields.get(6).unwrap_or(&0),
    })
}

// ─── Auto-detect card size ───
//
// /sys/block/<dev>/size contains the device size in 512-byte sectors.
// Multiply by 512 to get bytes.

fn detect_card_bytes(device: &str) -> Result<u64> {
    let path = format!("/sys/block/{}/size", device);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Cannot read card size from {path}"))?;
    let sectors: u64 = content.trim().parse()
        .with_context(|| format!("Cannot parse card size from {path}"))?;
    Ok(sectors * 512)
}

// ─── Auto-detect erase block size ───
//
// We try two sysfs queue attributes in order of preference:
//
//   1. discard_granularity — on cards supporting TRIM/discard this directly
//      reflects the erase block size the controller exposes
//   2. optimal_io_size — the controller's preferred IO alignment, often
//      matches the erase block size
//
// Both are in bytes. If neither gives a sensible value (> 0 and a power of 2)
// we fall back to 4 MB (4096 KB) which is a reasonable default for modern
// consumer SD cards.
//
// The user can always override with --erase-block-kb if they know better.

fn detect_erase_block_bytes(device: &str) -> u64 {
    // Fallback default: 4 MB erase block
    let fallback: u64 = 4096 * 1024;

    // Helper: read a sysfs queue attribute as u64 bytes
    let read_queue_attr = |attr: &str| -> Option<u64> {
        let path = format!("/sys/block/{}/queue/{}", device, attr);
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&v| v > 0 && v.is_power_of_two())
    };

    // Try discard_granularity first, then optimal_io_size
    read_queue_attr("discard_granularity")
        .or_else(|| read_queue_attr("optimal_io_size"))
        .unwrap_or(fallback)
}

// ─── Auto-detect mount points for this device ───
//
// Strategy:
//   1. List partition subdirectories under /sys/block/<dev>/ that match
//      the device name prefix (e.g. mmcblk0p1, mmcblk0p2). Also include
//      the raw device itself in case it is mounted directly without partitions.
//   2. Parse /proc/mounts to find which of those partitions are mounted
//      and at which mount points.
//   3. Return a list of (partition, mount_point) pairs.

fn find_mount_points(device: &str) -> Vec<(String, String)> {
    // Build the set of candidate device names: the device itself plus its partitions
    let mut candidates: Vec<String> = vec![format!("/dev/{}", device)];

    // Scan /sys/block/<dev>/ for partition subdirectories
    let sys_path = format!("/sys/block/{}", device);
    if let Ok(entries) = fs::read_dir(&sys_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Partition directories start with the device name (e.g. mmcblk0p1)
            if name.starts_with(device) {
                candidates.push(format!("/dev/{}", name));
            }
        }
    }

    // Parse /proc/mounts: each line is "<device> <mountpoint> <fstype> <options> 0 0"
    let mounts_content = match fs::read_to_string("/proc/mounts") {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut result = Vec::new();
    for line in mounts_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let mount_dev = parts[0];
        let mount_point = parts[1];

        // Check if this mount entry matches one of our candidate device paths
        if candidates.iter().any(|c| c == mount_dev) {
            result.push((mount_dev.to_string(), mount_point.to_string()));
        }
    }

    result
}

// ─── Filesystem fullness ───
//
// For each mount point, use statvfs to determine how full the filesystem is.
// Returns the highest usage fraction across all mounted partitions of the device.
// We use the worst (most full) partition because GC pressure is a card-wide
// phenomenon — a nearly-full partition stresses the entire FTL.

fn max_filesystem_fullness(mount_points: &[(String, String)]) -> f64 {
    let mut max_used: f64 = 0.0;

    for (_, mount_point) in mount_points {
        // statvfs gives us block counts for the filesystem
        if let Ok(stat) = statvfs(mount_point.as_str()) {
            let total = stat.blocks() as f64 * stat.block_size() as f64;
            let free = stat.blocks_free() as f64 * stat.block_size() as f64;
            if total > 0.0 {
                let used_frac = 1.0 - (free / total);
                if used_frac > max_used {
                    max_used = used_frac;
                }
            }
        }
    }

    max_used
}

// ─── Fullness WAF multiplier ───
//
// As the card fills up, the FTL has less free space to work with.
// Garbage collection is forced to run more frequently and with less
// choice about which blocks to consolidate, increasing WAF.
//
// These multipliers are heuristic — real values depend on the FTL
// implementation which is proprietary. The curve is intentionally
// conservative (not alarmist).
//
//   < 50% full  → no extra pressure, multiplier = 1.0
//   50-70%      → mild pressure,     multiplier = 1.1
//   70-85%      → moderate pressure, multiplier = 1.3
//   85-95%      → high pressure,     multiplier = 1.6
//   > 95%       → severe pressure,   multiplier = 2.0

fn fullness_waf_multiplier(used_fraction: f64) -> f64 {
    match used_fraction {
        f if f >= 0.95 => 2.0,
        f if f >= 0.85 => 1.6,
        f if f >= 0.70 => 1.3,
        f if f >= 0.50 => 1.1,
        _ => 1.0,
    }
}

// ─── Write amplification estimation ───
//
// The only thing we can observe from outside the SD card is:
//   - How many IOs the host sent
//   - How many sectors total
//   - Therefore average IO size
//
// Small random writes cause the FTL to do read-modify-erase-write
// on entire erase blocks. Large sequential writes map ~1:1.
//
// We model base WAF as a function of (avg_io_size / erase_block_size),
// then apply a fullness multiplier to account for GC pressure.

fn estimate_waf(
    delta_sectors: u64,
    delta_ios: u64,
    erase_block_bytes: u64,
    fullness_multiplier: f64,
) -> f64 {
    if delta_ios == 0 || delta_sectors == 0 {
        return 1.0;
    }

    let avg_io_bytes = (delta_sectors * 512) as f64 / delta_ios as f64;

    // Base WAF from IO size vs erase block size
    let base_waf = if avg_io_bytes >= erase_block_bytes as f64 {
        // Writes are already erase-block sized or larger — nearly 1:1
        1.05
    } else {
        // Naive worst case: full erase block rewritten per small write
        let naive_waf = erase_block_bytes as f64 / avg_io_bytes;

        // Real FTLs use log-structured translation, so actual WAF is
        // much less than naive. Factor of ~0.1-0.2 is reasonable for
        // consumer SD cards with decent controllers.
        let ftl_efficiency = 0.15;
        let waf = 1.0 + (naive_waf - 1.0) * ftl_efficiency;
        waf.clamp(1.0, naive_waf)
    };

    // Apply fullness multiplier — higher card usage = more GC pressure = higher WAF
    base_waf * fullness_multiplier
}

// ─── Persistence ───

fn load_state(path: &PathBuf) -> State {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(State::new)
}

fn save_state(path: &PathBuf, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    fs::write(path, json)?;
    Ok(())
}

fn now_string() -> String {
    // Simple unix timestamp without pulling in chrono
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", d.as_secs())
}

// ─── Main loop ───

fn main() -> Result<()> {
    let args = Args::parse();

    // ── Auto-detect card size from /sys/block/<dev>/size ──
    let card_bytes = detect_card_bytes(&args.device)
        .with_context(|| format!("Failed to detect size of device {}", args.device))?;
    let card_gb = card_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

    // ── Erase block size: use override if provided, otherwise auto-detect ──
    let erase_block_bytes = match args.erase_block_kb {
        Some(kb) => {
            println!("Erase block:  {} KB (user override)", kb);
            kb * 1024
        }
        None => {
            let detected = detect_erase_block_bytes(&args.device);
            println!("Erase block:  {} KB (auto-detected)", detected / 1024);
            detected
        }
    };
    let erase_block_kb = erase_block_bytes / 1024;

    // ── Find all mount points for this device's partitions ──
    let mount_points = find_mount_points(&args.device);

    // Build a flat list of mount point strings for storage in state JSON
    let mount_point_strings: Vec<String> = mount_points
        .iter()
        .map(|(dev, mp)| format!("{} -> {}", dev, mp))
        .collect();

    // The save interval as a Duration for time-based comparison
    let save_interval = Duration::from_secs(args.save_interval);

    let mut state = load_state(&args.state_file);

    // ── Refresh descriptor fields on every startup ──
    // These are always re-detected from the system so the JSON stays accurate
    // even if the card is swapped or the daemon is restarted with different args.
    state.device = args.device.clone();
    state.card_size_gb = (card_gb * 10.0).round() / 10.0; // round to 1 decimal place
    state.card_size_bytes = card_bytes;
    state.erase_block_kb = erase_block_kb;
    state.pe_cycles_rated = args.pe_cycles;
    state.mount_points = mount_point_strings.clone();

    // Read current kernel counters
    let current = read_kernel_stats(&args.device)?;

    // Detect reboot: kernel counters reset to 0 (or less than our last snapshot)
    if current.write_sectors < state.last_kernel_write_sectors {
        eprintln!(
            "Reboot detected (kernel sectors {} < stored {}). Continuing cumulative tracking.",
            current.write_sectors, state.last_kernel_write_sectors
        );
        state.reboot_count += 1;
    }

    // Set baseline to current kernel counters
    state.last_kernel_write_sectors = current.write_sectors;
    state.last_kernel_write_ios = current.write_ios;

    println!("sdwear — SD Card Flash Wear Estimator");
    println!("Device:        /dev/{}", args.device);
    println!("Card size:     {:.1} GB (auto-detected)", card_gb);
    println!("Erase block:   {} KB", erase_block_kb);
    println!("Rated P/E:     {} cycles", args.pe_cycles);
    println!("State file:    {}", args.state_file.display());
    println!("Save interval: {} seconds", args.save_interval);

    // Report detected mount points
    if mount_points.is_empty() {
        println!("Mount points:  none detected");
    } else {
        for (dev, mp) in &mount_points {
            println!("Mount point:   {} → {}", dev, mp);
        }
    }

    println!(
        "Lifetime budget: {:.1} TB total flash writes",
        (card_bytes as f64 * args.pe_cycles as f64) / 1e12
    );
    println!("─────────────────────────────────────────────────────────────");
    println!(
        "Restored state: {:.6} avg P/E cycles, {:.4}% life remaining",
        state.estimated_avg_pe_cycles, state.estimated_life_remaining_pct
    );
    println!("─────────────────────────────────────────────────────────────");
    println!(
        "{:>10} {:>10} {:>8} {:>6} {:>8} {:>12} {:>8}",
        "Δ IOs", "Δ KB", "Avg KB", "WAF", "Full%", "Avg P/E", "Life %"
    );

    let mut prev = current;

    // Track wall-clock time since last save rather than counting ticks.
    // This means the save period is accurate regardless of poll interval setting.
    let mut last_save = Instant::now();

    loop {
        thread::sleep(Duration::from_secs(args.interval));

        let now = read_kernel_stats(&args.device)?;

        let delta_ios = now.write_ios.saturating_sub(prev.write_ios);
        let delta_sectors = now.write_sectors.saturating_sub(prev.write_sectors);

        if delta_ios > 0 || delta_sectors > 0 {
            let delta_host_bytes = delta_sectors * 512;
            let delta_kb = delta_host_bytes / 1024;
            let avg_io_kb = if delta_ios > 0 {
                delta_kb as f64 / delta_ios as f64
            } else {
                0.0
            };

            // Sample current filesystem fullness and compute WAF multiplier.
            // We re-sample every poll so the estimate tracks changing disk usage.
            let fullness = max_filesystem_fullness(&mount_points);
            let fullness_mult = fullness_waf_multiplier(fullness);

            // Update the fullness in state so it is current at next save
            state.filesystem_fullness_pct = (fullness * 1000.0).round() / 10.0;

            let waf = estimate_waf(delta_sectors, delta_ios, erase_block_bytes, fullness_mult);
            let delta_flash_bytes = (delta_host_bytes as f64 * waf) as u64;

            // Update cumulative state
            state.total_host_sectors_written += delta_sectors;
            state.total_host_write_ios += delta_ios;
            state.estimated_flash_bytes_written += delta_flash_bytes;
            state.last_kernel_write_sectors = now.write_sectors;
            state.last_kernel_write_ios = now.write_ios;
            state.last_updated = now_string();

            // ★ THE KEY NUMBER ★
            state.estimated_avg_pe_cycles =
                state.estimated_flash_bytes_written as f64 / card_bytes as f64;

            state.estimated_life_remaining_pct =
                ((1.0 - (state.estimated_avg_pe_cycles / args.pe_cycles as f64)) * 100.0)
                    .clamp(0.0, 100.0);

            println!(
                "{:>10} {:>10} {:>8.1} {:>6.2} {:>7.1}% {:>12.6} {:>7.4}%",
                delta_ios,
                delta_kb,
                avg_io_kb,
                waf,
                fullness * 100.0,
                state.estimated_avg_pe_cycles,
                state.estimated_life_remaining_pct,
            );
        }

        prev = now;

        // Save state to disk when the configured save interval has elapsed.
        // Using wall-clock time (Instant) rather than tick counting means the
        // save period is accurate regardless of what --interval is set to.
        if last_save.elapsed() >= save_interval {
            save_state(&args.state_file, &state)?;
            last_save = Instant::now();
        }
    }
}
