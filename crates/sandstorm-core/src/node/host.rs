//! What this machine can do for density, found out rather than assumed.
//!
//! Both levers here are plain Linux and plain runsc -- no CPU feature, no
//! kernel patch, no particular cloud -- but whether each one *helps*
//! depends on the machine, so each is probed at startup and either turned
//! on or left off with the reason logged. A node where neither applies
//! behaves exactly as it did before.
//!
//! Measured on bm (2 vCPU, 3.6 GiB, ~110 MB/s cloud disk, kernel 7.0) with
//! a 376 MiB Python heap -- 40 MiB random, the rest objects and JSON:
//!
//! | frozen, as before                          | 376 MiB held | thaw + touch all:   60 ms |
//! | frozen + reclaim into zswap, no writeback  | 113 MiB held | thaw + touch all:  310 ms |
//! | frozen + reclaim, writeback allowed        |  13-115 MiB  | thaw + touch all: 0.4-3.9 s |
//!
//! | snapshot image, uncompressed | 363 MiB | restore cold 3.3 s, warm 138 ms |
//! | snapshot image, flate        |  84 MiB | restore cold 1.4 s, warm 696 ms |
//!
//! Also tried and not adopted, because they did nothing there: `runsc
//! restore --background` (returns early, but the app cannot run until the
//! whole image is in -- 2.06 s either way), `--exclude-committed-zero-pages`
//! (gVisor already leaves zero pages out), and `--direct` (3.2 s warm,
//! since it bypasses the page cache that makes warm restores fast).

use std::path::Path;
use std::time::Instant;

/// Whether to use a lever: decided by probing, or forced either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lever {
    #[default]
    Auto,
    On,
    Off,
}

impl std::str::FromStr for Lever {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            _ => Err(format!("{s}: expected auto, on or off")),
        }
    }
}

/// Swap that lands in compressed RAM, if this machine has any.
///
/// That is what makes reclaiming a frozen sandbox worth it. Reclaimed into
/// zswap or zram, its pages come back at memory speed -- 310 ms for all of
/// a 376 MiB heap on bm. Reclaimed onto a disk swap file they come back at
/// disk speed, 3.9 s, which is worse than a snapshot restore and gains
/// nothing the kernel would not do by itself under pressure.
pub fn compressed_swap() -> Option<&'static str> {
    let swaps = std::fs::read_to_string("/proc/swaps").ok()?;
    let devices: Vec<&str> = swaps
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    if devices.is_empty() {
        return None;
    }
    if devices.iter().any(|d| d.starts_with("/dev/zram")) {
        return Some("zram");
    }
    zswap_enabled().then_some("zswap")
}

/// Make sure reclaimed pages have compressed RAM to go to, on a machine
/// that has none, by the least intrusive route that works:
///
/// - **zswap already on, or zram already in use:** nothing to do.
/// - **disk swap, zswap off:** turn zswap on. This is what the load-test
///   fleet looked like -- a 2 GiB `/swap.img` on every node, zswap off --
///   and it left both reclaim levers off on all of them. zswap sits in
///   front of the swap that is already there and only changes what happens
///   to pages the kernel was about to write to it: they are compressed in
///   RAM first and written later, if at all. A runtime switch, back to the
///   boot default on reboot.
/// - **no swap at all:** add a zram device, swap in compressed RAM with no
///   disk behind it. Only through `zram-control/hot_add`, so a device the
///   host already uses is never touched. Sized at the machine's RAM in
///   logical capacity, with `mem_limit` capping what it may occupy at a
///   quarter of RAM -- incompressible pages cost zram more than they save,
///   and the cap keeps that from turning into thrash.
///
/// Either change is host-wide: with compressed swap in place the kernel
/// may compress the host's own idle pages too, not only sandboxes'. That is
/// the trade, and `--compressed-swap off` declines it. Every failure --
/// no zram in the kernel, no permission to load it in a container, a
/// read-only sysfs -- is a skip with the reason logged, never half-done.
/// Left in place when sandstorm stops and found again on the next start.
pub fn bootstrap_compressed_swap(lever: Lever) -> (bool, String) {
    if lever == Lever::Off {
        return (false, "turned off".into());
    }
    let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    let devices: Vec<&str> = swaps
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    if devices.iter().any(|d| d.starts_with("/dev/zram")) {
        return (false, "zram swap already in use".into());
    }
    if !devices.is_empty() {
        if zswap_enabled() {
            return (
                false,
                format!("zswap already on in front of {}", devices[0]),
            );
        }
        let knob = "/sys/module/zswap/parameters/enabled";
        return match std::fs::write(knob, "Y") {
            Ok(()) if zswap_enabled() => {
                (true, format!("turned zswap on in front of {}", devices[0]))
            }
            Ok(()) => (false, "zswap would not turn on; reclaim stays off".into()),
            Err(e) => (
                false,
                format!("could not turn zswap on ({e}); reclaim stays off"),
            ),
        };
    }
    match make_zram() {
        Ok(what) => (true, what),
        Err(e) => (false, format!("{e:#}; reclaim stays off")),
    }
}

fn zswap_enabled() -> bool {
    std::fs::read_to_string("/sys/module/zswap/parameters/enabled")
        .map(|s| matches!(s.trim(), "Y" | "1"))
        .unwrap_or(false)
}

fn make_zram() -> anyhow::Result<String> {
    use anyhow::{bail, Context};
    let control = Path::new("/sys/class/zram-control");
    if !control.exists() {
        // num_devices=0: only devices we ask for, not a stray zram0.
        let _ = std::process::Command::new("modprobe")
            .args(["zram", "num_devices=0"])
            .status();
    }
    if !control.exists() {
        bail!("no zram in this kernel, or no permission to load it");
    }
    let n: u32 = std::fs::read_to_string(control.join("hot_add"))
        .context("adding a zram device")?
        .trim()
        .parse()
        .context("reading the new zram device number")?;
    let dev = format!("/dev/zram{n}");
    let sys = std::path::PathBuf::from(format!("/sys/block/zram{n}"));
    let result = (|| {
        let offered = std::fs::read_to_string(sys.join("comp_algorithm")).unwrap_or_default();
        let comp = ["zstd", "lz4", "lzo-rle", "lzo"]
            .into_iter()
            .find(|c| {
                offered
                    .split_whitespace()
                    .any(|o| o.trim_matches(|ch| ch == '[' || ch == ']') == *c)
            })
            .unwrap_or("lzo");
        let _ = std::fs::write(sys.join("comp_algorithm"), comp);
        let ram = mem_total_bytes().context("reading MemTotal")?;
        std::fs::write(sys.join("disksize"), ram.to_string()).context("sizing zram")?;
        let _ = std::fs::write(sys.join("mem_limit"), (ram / 4).to_string());
        let ok =
            |cmd: &mut std::process::Command| cmd.status().map(|s| s.success()).unwrap_or(false);
        if !ok(std::process::Command::new("mkswap").arg(&dev)) {
            bail!("mkswap {dev} failed");
        }
        // Above any disk swap an operator adds later.
        if !ok(std::process::Command::new("swapon").args(["-p", "100", &dev])) {
            bail!("swapon {dev} failed");
        }
        Ok(format!(
            "made {dev}: {comp}, {} MiB logical, using at most {} MiB of RAM",
            ram >> 20,
            (ram / 4) >> 20
        ))
    })();
    if result.is_err() {
        let _ = std::fs::write(control.join("hot_remove"), n.to_string());
    }
    result
}

/// Namespaces to keep built on this machine, when nothing says otherwise.
pub fn default_netns_pool() -> usize {
    netns_pool_for(mem_total_bytes().unwrap_or(0))
}

/// Four for each GiB of memory, which is what a machine can run at once more
/// than what it costs: each is about 0.4 MiB of kernel memory. Past the pool a
/// sandbox builds its own at about 80 ms, one at a time for want of the RTNL
/// lock, whatever else runs in parallel: on bare metal with 16 threads, 100
/// children of a fork took 7.9 s with 32 and 1.6 s with 128.
fn netns_pool_for(ram_bytes: u64) -> usize {
    ((ram_bytes >> 30) as usize * 4).clamp(32, 256)
}

fn mem_total_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

/// Whether reclaimed sandbox pages should be pinned in zswap's compressed
/// pool rather than written on to the swap device behind it.
///
/// Yes when that device is a disk -- that is the 310 ms against 3.9 s on
/// bm. No when it is zram: there the device IS compressed RAM, and with
/// writeback off a page zswap declines could not go to it at all.
pub fn pin_in_zswap() -> bool {
    compressed_swap() == Some("zswap")
}

/// Decide whether to reclaim frozen sandboxes. Returns the decision and
/// why, for the startup log.
pub fn decide_reclaim(lever: Lever) -> (bool, String) {
    match (lever, compressed_swap()) {
        (Lever::Off, _) => (false, "turned off".into()),
        (Lever::On, Some(kind)) => (true, format!("forced on; swap is {kind}")),
        (Lever::On, None) => (
            true,
            "forced on, but no compressed swap: frozen pages will go to disk".into(),
        ),
        (Lever::Auto, Some(kind)) => (true, format!("swap is {kind}")),
        (Lever::Auto, None) => (
            false,
            "no zswap or zram; reclaiming to disk swap would thaw slower than a snapshot restores"
                .into(),
        ),
    }
}

/// Below this read throughput, a compressed snapshot restores faster from
/// a cold cache than an uncompressed one.
///
/// From bm: flate cut a 363 MiB image to 84 MiB and decompressed at about
/// 650 MB/s of output. Restoring uncompressed takes 363/R s, compressed
/// about 0.56 + 84/R, and the two meet at R ≈ 500 MB/s. Cloud block disks
/// are mostly well under that; local NVMe is well over it.
pub const COMPRESS_BELOW_MBPS: f64 = 500.0;

/// Sequential read throughput of the filesystem holding `dir`, uncached.
///
/// Writes a 64 MiB file, has the kernel drop it from the page cache, and
/// times reading it back. `posix_fadvise(DONTNEED)` on a synced file is
/// how any process can do that without root or `drop_caches`, and it works
/// on every Linux filesystem.
pub fn disk_read_mbps(dir: &Path) -> std::io::Result<f64> {
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;

    const SIZE: usize = 64 * 1024 * 1024;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(".sandstorm-disk-probe");
    let result = (|| {
        let mut f = std::fs::File::create(&path)?;
        // Not zeros: a filesystem that compresses or dedups would make
        // zeros look impossibly fast.
        let mut block = vec![0u8; 1024 * 1024];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for chunk in block.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
        }
        for i in 0..SIZE / block.len() {
            block[0] = i as u8;
            f.write_all(&block)?;
        }
        f.sync_all()?;
        drop_cached(f.as_raw_fd());

        let mut f = std::fs::File::open(&path)?;
        drop_cached(f.as_raw_fd());
        let t0 = Instant::now();
        let mut n = 0;
        loop {
            let r = f.read(&mut block)?;
            if r == 0 {
                break;
            }
            n += r;
        }
        let secs = t0.elapsed().as_secs_f64().max(1e-6);
        Ok(n as f64 / 1e6 / secs)
    })();
    let _ = std::fs::remove_file(&path);
    result
}

fn drop_cached(fd: i32) {
    // SAFETY: a plain advisory syscall on a descriptor we own; it has no
    // memory-safety preconditions and its failure only makes the probe
    // read from cache, which the caller sees as a fast disk.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = fd;
}

/// The band either side of `COMPRESS_BELOW_MBPS` a node must cross to
/// change its mind.
///
/// The load-test fleet read 486, 494, 498 and 645 MB/s: a bare threshold at
/// 500 sat right on top of it, so a node could compress on one start and
/// not on the next. Formats mixing across nodes is harmless -- an image is
/// only ever restored on the node that wrote it, and restore reads the
/// format from the image -- but a node flapping is noise, so a decision
/// once made is kept until a reading lands clearly on the other side.
pub const COMPRESS_HYSTERESIS_MBPS: f64 = 100.0;

/// The decision, given the disk reading and what this node decided last.
pub fn compress_given(mbps: f64, before: Option<bool>) -> bool {
    match before {
        Some(true) => mbps < COMPRESS_BELOW_MBPS + COMPRESS_HYSTERESIS_MBPS,
        Some(false) => mbps < COMPRESS_BELOW_MBPS - COMPRESS_HYSTERESIS_MBPS,
        None => mbps < COMPRESS_BELOW_MBPS,
    }
}

/// Decide whether to compress snapshots. Returns the decision and why.
pub fn decide_compression(lever: Lever, work_dir: &Path) -> (bool, String) {
    let memo = work_dir.join("compress-snapshots");
    let before =
        std::fs::read_to_string(&memo)
            .ok()
            .and_then(|s| match s.split_whitespace().next() {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            });
    let (on, why) = match lever {
        Lever::Off => (false, "turned off".to_string()),
        Lever::On => (true, "forced on".to_string()),
        Lever::Auto => match disk_read_mbps(work_dir) {
            Ok(mbps) => {
                let on = compress_given(mbps, before);
                let held = before.is_some_and(|b| b == on) && on != (mbps < COMPRESS_BELOW_MBPS);
                (
                    on,
                    format!(
                        "disk reads {mbps:.0} MB/s against {COMPRESS_BELOW_MBPS:.0}{}",
                        if held {
                            ", inside the hysteresis band: kept as before"
                        } else {
                            ""
                        }
                    ),
                )
            }
            Err(e) => (false, format!("could not probe the disk ({e}); left off")),
        },
    };
    let _ = std::fs::write(&memo, if on { "on\n" } else { "off\n" });
    (on, why)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_namespace_pool_follows_memory_within_bounds() {
        use super::netns_pool_for;
        const GIB: u64 = 1 << 30;
        assert_eq!(netns_pool_for(0), 32, "memory not read");
        assert_eq!(netns_pool_for(2 * GIB), 32);
        assert_eq!(netns_pool_for(31 * GIB), 124);
        assert_eq!(netns_pool_for(512 * GIB), 256);
    }

    use super::*;

    #[test]
    fn compression_holds_its_decision_inside_the_band() {
        // The four fleet readings, each on a first start and on a restart
        // that had decided the other way.
        for mbps in [486.0, 494.0, 498.0] {
            assert!(compress_given(mbps, None));
            assert!(compress_given(mbps, Some(true)));
            assert!(!compress_given(mbps, Some(false)), "{mbps}: stays off");
        }
        assert!(!compress_given(645.0, None));
        assert!(!compress_given(645.0, Some(true)), "past the band: changes");
        assert!(compress_given(380.0, Some(false)), "past the band: changes");
    }

    #[test]
    fn the_compression_decision_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        decide_compression(Lever::On, dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("compress-snapshots")).unwrap(),
            "on\n"
        );
    }

    #[test]
    fn levers_parse() {
        assert_eq!("auto".parse::<Lever>(), Ok(Lever::Auto));
        assert_eq!("on".parse::<Lever>(), Ok(Lever::On));
        assert_eq!("off".parse::<Lever>(), Ok(Lever::Off));
        assert!("yes".parse::<Lever>().is_err());
    }

    #[test]
    fn off_and_on_are_not_second_guessed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!decide_reclaim(Lever::Off).0);
        assert!(decide_reclaim(Lever::On).0);
        assert!(!decide_compression(Lever::Off, dir.path()).0);
        assert!(decide_compression(Lever::On, dir.path()).0);
    }

    /// Off means not even a look: no modprobe, no device, no knob.
    #[test]
    fn compressed_swap_off_does_nothing() {
        let (made, why) = bootstrap_compressed_swap(Lever::Off);
        assert!(!made);
        assert_eq!(why, "turned off");
    }

    #[test]
    fn the_disk_probe_measures_something_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let mbps = disk_read_mbps(dir.path()).unwrap();
        assert!(mbps > 0.0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
