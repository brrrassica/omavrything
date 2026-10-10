//! Enumerate an NTFS volume's MFT directly from a device or image.
//!
//! ```sh
//! cargo run -p omavrything-ntfs --example scan -- /dev/nvme0n1p8 20
//! ```
//!
//! Reading a raw block device requires root or membership in the `disk` group.

use std::fs::File;

use omavrything_ntfs::NtfsVolume;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(path) => path,
        None => {
            eprintln!("usage: scan <device-or-image> [limit]");
            std::process::exit(2);
        }
    };
    let limit: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let file = match File::open(&path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("cannot open {path}: {err}");
            std::process::exit(1);
        }
    };
    let mut volume = match NtfsVolume::open(file) {
        Ok(volume) => volume,
        Err(err) => {
            eprintln!("not a readable NTFS volume: {err}");
            std::process::exit(1);
        }
    };

    println!("MFT records: {}", volume.record_count());
    let entries = match volume.enumerate() {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("enumerate failed: {err}");
            std::process::exit(1);
        }
    };
    println!("entries: {}", entries.len());
    for entry in entries.iter().take(limit) {
        let kind = if entry.is_dir { "d" } else { "f" };
        println!("{kind} {:>12} {} {}", entry.size, entry.mtime, entry.path);
    }
}
