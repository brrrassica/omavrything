//! A minimal synthetic NTFS image builder, used only by tests.
//!
//! It produces a volume with a valid boot sector, an `$MFT` record whose
//! `$DATA` run list locates the MFT, a root directory, and a small tree:
//!
//! ```text
//! /hello.txt
//! /subdir/
//! /subdir/world.txt
//! ```
//!
//! Only the structures the reader consumes are populated; directory index
//! structures are intentionally omitted.

use crate::mft::{ATTR_DATA, ATTR_FILE_NAME, ATTR_STANDARD_INFORMATION};

const BYTES_PER_SECTOR: u16 = 512;
const SECTORS_PER_CLUSTER: u8 = 8;
const CLUSTER_SIZE: usize = BYTES_PER_SECTOR as usize * SECTORS_PER_CLUSTER as usize;
const TOTAL_CLUSTERS: usize = 256;
const MFT_LCN: u64 = 4;
const MFT_CLUSTERS: u64 = 8;
const RECORD_SIZE: usize = 1024;
const MTIME: u64 = 1_700_000_000;

/// Build the synthetic image as a byte vector.
pub fn build_test_image() -> Vec<u8> {
    let mut img = vec![0u8; CLUSTER_SIZE * TOTAL_CLUSTERS];
    write_boot_sector(&mut img);

    let mft_off = MFT_LCN as usize * CLUSTER_SIZE;
    let mft_size = MFT_CLUSTERS * CLUSTER_SIZE as u64;

    // Record 0: $MFT, with a non-resident $DATA run list covering the MFT.
    let mft_data = nonresident_data(&[(MFT_LCN, MFT_CLUSTERS)], mft_size, 0);
    let mft_name = resident_attr(
        ATTR_FILE_NAME,
        &file_name_value(5, "$MFT", 0, mft_size, false),
        1,
    );
    let rec0 = build_record(0, false, &[mft_name, mft_data]);

    // Record 5: the root directory.
    let root_name = resident_attr(ATTR_FILE_NAME, &file_name_value(5, ".", 0, 0, true), 1);
    let rec5 = build_record(5, true, &[root_name]);

    // Record 24: /hello.txt. The `$FILE_NAME` mtime deliberately differs from
    // `$STANDARD_INFORMATION`, and a named `$DATA` stream is present, so the
    // test verifies the authoritative sources win.
    let hello_si = resident_attr(ATTR_STANDARD_INFORMATION, &standard_information(MTIME), 0);
    let hello_name = resident_attr(
        ATTR_FILE_NAME,
        &file_name_value(5, "hello.txt", MTIME - 1000, 5, false),
        1,
    );
    let hello_data = resident_attr(ATTR_DATA, b"hello", 2);
    let hello_stream = named_resident_attr(ATTR_DATA, "Zone.Identifier", &[0u8; 99], 3);
    let rec24 = build_record(24, false, &[hello_si, hello_name, hello_data, hello_stream]);

    // Record 25: /subdir
    let sub_name = resident_attr(
        ATTR_FILE_NAME,
        &file_name_value(5, "subdir", MTIME, 0, true),
        1,
    );
    let rec25 = build_record(25, true, &[sub_name]);

    // Record 26: /subdir/world.txt
    let world_name = resident_attr(
        ATTR_FILE_NAME,
        &file_name_value(25, "world.txt", MTIME, 3, false),
        1,
    );
    let world_data = resident_attr(ATTR_DATA, b"abc", 2);
    let rec26 = build_record(26, false, &[world_name, world_data]);

    for (number, rec) in [
        (0u64, &rec0),
        (5, &rec5),
        (24, &rec24),
        (25, &rec25),
        (26, &rec26),
    ] {
        let off = mft_off + number as usize * RECORD_SIZE;
        img[off..off + rec.len()].copy_from_slice(rec);
    }

    img
}

fn write_boot_sector(img: &mut [u8]) {
    let b = &mut img[..512];
    b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    b[0x0B..0x0D].copy_from_slice(&BYTES_PER_SECTOR.to_le_bytes());
    b[0x0D] = SECTORS_PER_CLUSTER;
    b[0x15] = 0xF8;
    let total_sectors = (TOTAL_CLUSTERS * SECTORS_PER_CLUSTER as usize) as u64;
    b[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    b[0x30..0x38].copy_from_slice(&MFT_LCN.to_le_bytes());
    b[0x38..0x40].copy_from_slice(&16u64.to_le_bytes());
    b[0x40] = (-10i8) as u8; // 2^10 = 1024-byte records
    b[0x44] = 1;
    b[0x48..0x50].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
    b[0x1FE] = 0x55;
    b[0x1FF] = 0xAA;
}

/// Build a 1024-byte FILE record with the given attributes and a valid fixup.
fn build_record(number: u64, is_dir: bool, attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut rec = vec![0u8; RECORD_SIZE];
    rec[0..4].copy_from_slice(b"FILE");
    rec[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes()); // USA offset
    rec[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // USA count
    rec[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
    rec[0x12..0x14].copy_from_slice(&1u16.to_le_bytes()); // link count
    rec[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // attributes offset
    let flags: u16 = 0x0001 | if is_dir { 0x0002 } else { 0 };
    rec[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
    rec[0x1C..0x20].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
    rec[0x28..0x2A].copy_from_slice(&(attrs.len() as u16 + 1).to_le_bytes());
    rec[0x2C..0x30].copy_from_slice(&(number as u32).to_le_bytes());

    let mut off = 0x38;
    for a in attrs {
        rec[off..off + a.len()].copy_from_slice(a);
        off += a.len();
    }
    rec[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    off += 4;
    rec[0x18..0x1C].copy_from_slice(&(off as u32).to_le_bytes()); // used size

    finalize_record(&mut rec, 0x0001);
    rec
}

/// Write the update sequence array and stamp the sequence number into each
/// sector's last two bytes.
fn finalize_record(rec: &mut [u8], usn: u16) {
    let sectors = rec.len() / BYTES_PER_SECTOR as usize;
    rec[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
    for i in 0..sectors {
        let end = (i + 1) * BYTES_PER_SECTOR as usize;
        let orig = [rec[end - 2], rec[end - 1]];
        rec[0x32 + i * 2..0x34 + i * 2].copy_from_slice(&orig);
        rec[end - 2..end].copy_from_slice(&usn.to_le_bytes());
    }
}

/// Build a `$STANDARD_INFORMATION` value with the given modification time.
fn standard_information(mtime: u64) -> Vec<u8> {
    let mut v = vec![0u8; 0x48];
    let ticks = (mtime as i64 + 11_644_473_600) as u64 * 10_000_000;
    for off in [0x00usize, 0x08, 0x10, 0x18] {
        v[off..off + 8].copy_from_slice(&ticks.to_le_bytes());
    }
    v[0x20..0x24].copy_from_slice(&0x20u32.to_le_bytes()); // FILE_ATTRIBUTE_ARCHIVE
    v
}

/// Build a named resident attribute (e.g. an alternate `$DATA` stream).
fn named_resident_attr(attr_type: u32, name: &str, value: &[u8], id: u16) -> Vec<u8> {
    let name_utf16: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = name_utf16.len() * 2;
    let name_off = 0x18usize;
    let value_off = (name_off + name_bytes + 7) & !7;
    let len = (value_off + value.len() + 7) & !7;
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&attr_type.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 0; // resident
    a[9] = name_utf16.len() as u8;
    a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
    a[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
    a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
    for (i, u) in name_utf16.iter().enumerate() {
        a[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
    a[value_off..value_off + value.len()].copy_from_slice(value);
    a
}

/// Build a resident attribute (header 0x18, 8-byte aligned).
fn resident_attr(attr_type: u32, value: &[u8], id: u16) -> Vec<u8> {
    let value_off = 0x18usize;
    let len = (value_off + value.len() + 7) & !7;
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&attr_type.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 0; // resident
    a[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
    a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
    a[value_off..value_off + value.len()].copy_from_slice(value);
    a
}

/// Build a non-resident `$DATA` attribute (header 0x40, 8-byte aligned).
fn nonresident_data(runs: &[(u64, u64)], real_size: u64, id: u16) -> Vec<u8> {
    let run_bytes = encode_runs(runs);
    let run_off = 0x40usize;
    let len = (run_off + run_bytes.len() + 7) & !7;
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 1; // non-resident
    a[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
    let last_vcn = runs.iter().map(|(_, l)| l).sum::<u64>().saturating_sub(1);
    a[0x18..0x20].copy_from_slice(&last_vcn.to_le_bytes());
    a[0x20..0x22].copy_from_slice(&(run_off as u16).to_le_bytes());
    a[0x28..0x30].copy_from_slice(&real_size.to_le_bytes());
    a[0x30..0x38].copy_from_slice(&real_size.to_le_bytes());
    a[0x38..0x40].copy_from_slice(&real_size.to_le_bytes());
    a[run_off..run_off + run_bytes.len()].copy_from_slice(&run_bytes);
    a
}

/// Build a `$FILE_NAME` attribute value.
fn file_name_value(parent: u64, name: &str, mtime: u64, real_size: u64, is_dir: bool) -> Vec<u8> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    let mut v = vec![0u8; 0x42 + utf16.len() * 2];
    v[0..8].copy_from_slice(&parent.to_le_bytes());
    let ticks = (mtime as i64 + 11_644_473_600) as u64 * 10_000_000;
    for off in [0x08usize, 0x10, 0x18, 0x20] {
        v[off..off + 8].copy_from_slice(&ticks.to_le_bytes());
    }
    v[0x28..0x30].copy_from_slice(&real_size.to_le_bytes());
    v[0x30..0x38].copy_from_slice(&real_size.to_le_bytes());
    let flags: u32 = if is_dir { 0x1000_0000 } else { 0x20 };
    v[0x38..0x3C].copy_from_slice(&flags.to_le_bytes());
    v[0x40] = utf16.len() as u8;
    v[0x41] = 3; // Win32 & DOS namespace
    for (i, u) in utf16.iter().enumerate() {
        v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    v
}

/// Encode a run list.
fn encode_runs(runs: &[(u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for &(lcn, len) in runs {
        let len_bytes = minimal_unsigned(len);
        let off_bytes = minimal_signed(lcn as i64 - prev);
        out.push(((off_bytes.len() as u8) << 4) | (len_bytes.len() as u8));
        out.extend_from_slice(&len_bytes);
        out.extend_from_slice(&off_bytes);
        prev = lcn as i64;
    }
    out.push(0);
    out
}

fn minimal_unsigned(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while v > 0 {
        out.push((v & 0xFF) as u8);
        v >>= 8;
    }
    if out.is_empty() {
        out.push(0);
    }
    out
}

fn minimal_signed(v: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut val = v;
    loop {
        let byte = (val & 0xFF) as u8;
        out.push(byte);
        val >>= 8;
        if (val == 0 && byte & 0x80 == 0) || (val == -1 && byte & 0x80 != 0) {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_has_a_valid_boot_sector() {
        let img = build_test_image();
        assert!(crate::boot::is_ntfs(&img));
        let bs = crate::boot::BootSector::parse(&img).unwrap();
        assert_eq!(bs.mft_lcn, MFT_LCN);
        assert_eq!(bs.mft_record_size(), RECORD_SIZE as u64);
    }

    #[test]
    fn record_count_matches_the_mft_size() {
        let img = build_test_image();
        let bs = crate::boot::BootSector::parse(&img).unwrap();
        let mft_size = MFT_CLUSTERS * bs.cluster_size();
        let expected = MFT_CLUSTERS * CLUSTER_SIZE as u64 / RECORD_SIZE as u64;
        assert_eq!(mft_size / bs.mft_record_size(), expected);
    }
}
