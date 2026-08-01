//! MILESTONE 18: a minimal filesystem on top of Milestone 11's raw ATA
//! disk I/O -- until now only one fixed sector (LBA 0) could be
//! persisted (the learned weights); this adds real named-file storage,
//! a genuine "you can save your work" capability.
//!
//! MILESTONE 22: multi-sector files and real delete/reclamation. Each
//! of the 8 directory entries (still at LBA 1) now also stores a
//! start LBA and a sector count, allocated from a shared 64-sector
//! data pool at LBA 2..66 (first-fit over contiguous free runs
//! computed from the currently-used entries, not a persisted
//! bitmap) -- so files genuinely span multiple sectors up to a real
//! cap of MAX_FILE_SECTORS (8) * 512 = 4096 bytes, and `rm` actually
//! frees an entry's sectors back into that pool for a later `write`
//! to reuse, rather than just marking a slot deleted forever.
//!
//! MILESTONE 28: real one-level subdirectories. A directory entry can
//! now be either a file or a subdirectory (a new `is_dir` flag on each
//! entry); a subdirectory's "data" is exactly ONE sector, allocated
//! from the SAME shared pool files already use, holding its own
//! 8-entry table in the identical on-disk format as the root table at
//! LBA 1. `mkdir NAME` only creates directories directly under the
//! root (no `mkdir a/b`), and `write`/`read`/`rm` accept one optional
//! `DIR/` prefix (no `a/b/c`) -- honest, disclosed depth cap of 1,
//! matching the same "deliberately minimal" spirit as the 4096-byte
//! file cap above. Because a subdirectory's table is capped at one
//! 512-byte sector, it holds at most the same 8 entries as root, and
//! because subdirectory entries are never themselves allowed to be
//! directories, there is no recursive-occupancy scan to write --
//! `collect_occupied` only ever needs to look one level deep. `rm`
//! refuses to remove a directory entry outright (empty or not) --
//! there is no rmdir/recursive-delete yet, so refusing unconditionally
//! is the honest, safe choice rather than silently orphaning a
//! subdirectory's own pool sector or its files' sectors.
//!
//! Still deliberately minimal, disclosed rather than hidden: no
//! fragmentation-avoiding allocator and no directory/pool compaction
//! -- a disk that has accumulated enough delete/write churn can fail
//! an allocation with "not enough free disk space" even when the
//! total free sectors would suffice, because free space isn't
//! necessarily contiguous and nothing moves existing files to make
//! room. Subdirectories eat into that same shared pool too (one
//! sector per `mkdir`), so heavy subdirectory use leaves less pool
//! space for file data.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

const DIR_LBA: u32 = 1;
const MAX_ENTRIES: usize = 8;
const FILE_DATA_START_LBA: u32 = 2;
const MAGIC: u32 = 0x53504B46; // "SPKF"
const NAME_LEN: usize = 16;
const ENTRY_LEN: usize = NAME_LEN + 1 + 1 + 2 + 4 + 1; // name + used-flag + is_dir-flag + u16 len + u32 start_lba + u8 sector_count
const SECTOR_SIZE: usize = 512;
const MAX_FILE_SECTORS: usize = 8; // real per-file cap: 8 * 512 = 4096 bytes
const MAX_FILE_BYTES: usize = MAX_FILE_SECTORS * SECTOR_SIZE;
const NUM_DATA_SECTORS: usize = MAX_ENTRIES * MAX_FILE_SECTORS; // shared pool, used by both file data AND subdirectory tables

#[derive(Clone, Copy)]
struct DirEntry {
    used: bool,
    is_dir: bool,
    name: [u8; NAME_LEN],
    len: u16,
    start_lba: u32,
    sector_count: u8,
}

impl DirEntry {
    fn empty() -> Self {
        DirEntry { used: false, is_dir: false, name: [0; NAME_LEN], len: 0, start_lba: 0, sector_count: 0 }
    }

    fn name_str(&self) -> String {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        String::from_utf8_lossy(&self.name[..end]).to_string()
    }
}

// Loads a directory table (root OR a subdirectory's own one-sector
// table) from an arbitrary LBA -- root always lives at the fixed
// DIR_LBA, a subdirectory's table lives wherever it was allocated out
// of the shared data pool, recorded as that subdirectory's own
// start_lba in its parent's entry.
fn load_dir_at(lba: u32) -> [DirEntry; MAX_ENTRIES] {
    let mut buf = [0u8; 512];
    let mut entries = [DirEntry::empty(); MAX_ENTRIES];
    if crate::ata::read_sector(lba, &mut buf).is_err() {
        return entries;
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return entries; // uninitialized disk -- empty directory, not an error
    }
    for i in 0..MAX_ENTRIES {
        let off = 4 + i * ENTRY_LEN;
        let used = buf[off] != 0;
        let is_dir = buf[off + 1] != 0;
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&buf[off + 2..off + 2 + NAME_LEN]);
        let len = u16::from_le_bytes(buf[off + 2 + NAME_LEN..off + 4 + NAME_LEN].try_into().unwrap());
        let start_lba = u32::from_le_bytes(buf[off + 4 + NAME_LEN..off + 8 + NAME_LEN].try_into().unwrap());
        let sector_count = buf[off + 8 + NAME_LEN];
        entries[i] = DirEntry { used, is_dir, name, len, start_lba, sector_count };
    }
    entries
}

fn save_dir_at(lba: u32, entries: &[DirEntry; MAX_ENTRIES]) -> Result<(), &'static str> {
    let mut buf = [0u8; 512];
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    for (i, e) in entries.iter().enumerate() {
        let off = 4 + i * ENTRY_LEN;
        buf[off] = e.used as u8;
        buf[off + 1] = e.is_dir as u8;
        buf[off + 2..off + 2 + NAME_LEN].copy_from_slice(&e.name);
        buf[off + 2 + NAME_LEN..off + 4 + NAME_LEN].copy_from_slice(&e.len.to_le_bytes());
        buf[off + 4 + NAME_LEN..off + 8 + NAME_LEN].copy_from_slice(&e.start_lba.to_le_bytes());
        buf[off + 8 + NAME_LEN] = e.sector_count;
    }
    crate::ata::write_sector(lba, &buf)
}

fn name_bytes(name: &str) -> [u8; NAME_LEN] {
    let mut out = [0u8; NAME_LEN];
    let bytes = name.as_bytes();
    let n = bytes.len().min(NAME_LEN);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

// Splits a `write`/`read`/`rm` argument into the directory table it
// lives in (root, or a resolved one-level-deep subdirectory) and the
// leaf name within that table -- rejects anything past one level
// (`a/b/c`) with an honest error rather than silently only using the
// first two components.
fn resolve_table<'a>(path: &'a str) -> Result<(u32, &'a str), String> {
    match path.split_once('/') {
        Some((dir, name)) => {
            if name.is_empty() || name.contains('/') {
                return Err(format!(
                    "'{path}' -- only one level of subdirectory nesting is supported (DIR/NAME, not DIR/SUB/NAME)"
                ));
            }
            Ok((resolve_dir_lba(dir)?, name))
        }
        None => Ok((DIR_LBA, path)),
    }
}

fn resolve_dir_lba(dir_name: &str) -> Result<u32, String> {
    let root = load_dir_at(DIR_LBA);
    let target = name_bytes(dir_name);
    let entry = root
        .iter()
        .find(|e| e.used && e.name == target)
        .ok_or_else(|| format!("no such directory '{dir_name}'"))?;
    if !entry.is_dir {
        return Err(format!("'{dir_name}' is a file, not a directory"));
    }
    Ok(entry.start_lba)
}

// Every currently-occupied pool sector, disk-wide -- both files' data
// AND every subdirectory's own one-sector table -- so allocation never
// hands out a sector already claimed by some OTHER directory's entry.
// Subdirectory entries are never themselves directories (one level of
// nesting only), so this only ever needs to recurse one level deep.
// `exclude`, when given, is the (start_lba, sector_count) of the
// allocation currently being replaced in-place, so rewriting an
// existing file/subdir doesn't see its own old sectors as busy.
fn collect_occupied(exclude: Option<(u32, u8)>) -> Vec<(u32, u8)> {
    let mut ranges = Vec::new();
    let root = load_dir_at(DIR_LBA);
    for e in root.iter() {
        if !e.used || e.sector_count == 0 {
            continue;
        }
        ranges.push((e.start_lba, e.sector_count));
        if e.is_dir {
            for se in load_dir_at(e.start_lba).iter() {
                if se.used && se.sector_count > 0 {
                    ranges.push((se.start_lba, se.sector_count));
                }
            }
        }
    }
    if let Some(ex) = exclude {
        if let Some(pos) = ranges.iter().position(|&r| r == ex) {
            ranges.remove(pos);
        }
    }
    ranges
}

// First-fit scan over the shared data pool (pool-relative sector
// indices, 0..NUM_DATA_SECTORS) for a run of `needed` free sectors,
// against a disk-wide occupancy list from collect_occupied.
fn find_free_span(occupied: &[(u32, u8)], needed: usize) -> Option<u32> {
    'outer: for start in 0..=(NUM_DATA_SECTORS - needed) {
        let cand_start = FILE_DATA_START_LBA + start as u32;
        let cand_end = cand_start + needed as u32;
        for &(occ_start, occ_count) in occupied {
            let occ_end = occ_start + occ_count as u32;
            if cand_start < occ_end && occ_start < cand_end {
                continue 'outer;
            }
        }
        return Some(start as u32);
    }
    None
}

pub fn make_dir(name: &str) -> Result<(), String> {
    if name.contains('/') {
        return Err(
            "mkdir only supports creating a directory directly under the root (no 'mkdir a/b')".to_string(),
        );
    }
    let mut root = load_dir_at(DIR_LBA);
    let target_name = name_bytes(name);

    if root.iter().any(|e| e.used && e.name == target_name) {
        return Err(format!("'{name}' already exists"));
    }
    let slot = root.iter().position(|e| !e.used).ok_or_else(|| "directory full (max 8 entries)".to_string())?;

    let occupied = collect_occupied(None);
    let start_pool =
        find_free_span(&occupied, 1).ok_or_else(|| "not enough free disk space (fragmented or full)".to_string())?;
    let table_lba = FILE_DATA_START_LBA + start_pool;

    save_dir_at(table_lba, &[DirEntry::empty(); MAX_ENTRIES]).map_err(|e| e.to_string())?;

    root[slot] = DirEntry { used: true, is_dir: true, name: target_name, len: 0, start_lba: table_lba, sector_count: 1 };
    save_dir_at(DIR_LBA, &root).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn write_file(path: &str, data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file too large -- max {MAX_FILE_BYTES} bytes ({MAX_FILE_SECTORS} sectors per file, no bigger files yet)"
        ));
    }
    let (table_lba, name) = resolve_table(path)?;
    let mut entries = load_dir_at(table_lba);
    let target_name = name_bytes(name);

    let existing = entries.iter().position(|e| e.used && e.name == target_name);
    if let Some(i) = existing {
        if entries[i].is_dir {
            return Err(format!("'{name}' is a directory"));
        }
    }
    let slot = existing
        .or_else(|| entries.iter().position(|e| !e.used))
        .ok_or_else(|| "directory full (max 8 entries)".to_string())?;

    let old_alloc = if entries[slot].used { Some((entries[slot].start_lba, entries[slot].sector_count)) } else { None };
    let needed = if data.is_empty() { 0 } else { (data.len() + SECTOR_SIZE - 1) / SECTOR_SIZE };

    let start_pool = if needed == 0 {
        0
    } else {
        let occupied = collect_occupied(old_alloc);
        find_free_span(&occupied, needed).ok_or_else(|| "not enough free disk space (fragmented or full)".to_string())?
    };

    for i in 0..needed {
        let mut sector = [0u8; SECTOR_SIZE];
        let start = i * SECTOR_SIZE;
        let end = (start + SECTOR_SIZE).min(data.len());
        sector[..end - start].copy_from_slice(&data[start..end]);
        crate::ata::write_sector(FILE_DATA_START_LBA + start_pool + i as u32, &sector).map_err(|e| e.to_string())?;
    }

    entries[slot] = DirEntry {
        used: true,
        is_dir: false,
        name: target_name,
        len: data.len() as u16,
        start_lba: if needed == 0 { 0 } else { FILE_DATA_START_LBA + start_pool },
        sector_count: needed as u8,
    };
    save_dir_at(table_lba, &entries).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn read_file(path: &str) -> Result<Vec<u8>, String> {
    let (table_lba, name) = resolve_table(path)?;
    let entries = load_dir_at(table_lba);
    let target_name = name_bytes(name);
    let entry = entries.iter().find(|e| e.used && e.name == target_name).ok_or_else(|| format_no_such_file(name))?;
    if entry.is_dir {
        return Err(format!("'{name}' is a directory, not a file"));
    }

    if entry.sector_count == 0 {
        return Ok(Vec::new());
    }

    let mut data = Vec::with_capacity(entry.sector_count as usize * SECTOR_SIZE);
    let mut sector = [0u8; SECTOR_SIZE];
    for i in 0..entry.sector_count as u32 {
        crate::ata::read_sector(entry.start_lba + i, &mut sector).map_err(|e| e.to_string())?;
        data.extend_from_slice(&sector);
    }
    data.truncate(entry.len as usize);
    Ok(data)
}

pub fn delete_file(path: &str) -> Result<(), String> {
    let (table_lba, name) = resolve_table(path)?;
    let mut entries = load_dir_at(table_lba);
    let target_name = name_bytes(name);
    let slot = entries.iter().position(|e| e.used && e.name == target_name).ok_or_else(|| format_no_such_file(name))?;
    if entries[slot].is_dir {
        return Err(format!("'{name}' is a directory -- rm does not remove directories (no rmdir yet)"));
    }
    entries[slot] = DirEntry::empty(); // frees both the directory slot and its data-pool sectors for reuse
    save_dir_at(table_lba, &entries).map_err(|e| e.to_string())?;
    Ok(())
}

fn format_no_such_file(name: &str) -> String {
    alloc::format!("no such file '{name}'")
}

pub fn list(dir: Option<&str>) -> Result<Vec<(String, bool, u16)>, String> {
    let table_lba = match dir {
        Some(d) => resolve_dir_lba(d)?,
        None => DIR_LBA,
    };
    Ok(load_dir_at(table_lba).iter().filter(|e| e.used).map(|e| (e.name_str(), e.is_dir, e.len)).collect())
}
