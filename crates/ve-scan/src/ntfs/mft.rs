//! Pure parsing of the NTFS Master File Table over any `Read + Seek` source: a raw volume
//! handle or an image file. Compiled on every platform.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};

/// MFT record number of a Volume's root folder.
pub const ROOT_RECORD: u64 = 5;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FLAG_IN_USE: u16 = 0x01;
const FLAG_DIRECTORY: u16 = 0x02;
const ATTR_COMPRESSED: u16 = 0x0001;
const ATTR_SPARSE: u16 = 0x8000;

const NAMESPACE_DOS: u8 = 2;
/// Update-sequence stride: fixups always cover 512-byte blocks regardless of sector size.
const FIXUP_STRIDE: usize = 512;
/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;
const CHUNK_BYTES: u64 = 1 << 20;
const MAX_RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Low 48 bits of a file reference: the MFT record number.
pub fn record_number(reference: u64) -> u64 {
    reference & MAX_RECORD_NUMBER_MASK
}

/// Volume layout from the boot sector.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub record_size: u32,
    /// Byte offset of the first MFT record.
    pub mft_offset: u64,
}

pub fn read_geometry<R: Read + Seek>(reader: &mut R) -> io::Result<Geometry> {
    reader.seek(SeekFrom::Start(0))?;
    // Volume handles need whole sectors; 4096 covers every sector size and a short image
    // still yields its first 512 bytes.
    let mut boot = vec![0u8; 4096];
    let mut got = 0;
    while got < boot.len() {
        match reader.read(&mut boot[got..])? {
            0 => break,
            n => got += n,
        }
    }
    if got < 512 || &boot[3..11] != b"NTFS    " {
        return Err(invalid("not an NTFS boot sector"));
    }
    let bytes_per_sector = u32::from(u16_at(&boot, 0x0B));
    if !(512..=4096).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
        return Err(invalid("unsupported NTFS sector size"));
    }
    let spc = boot[0x0D];
    let sectors_per_cluster = if spc > 0x80 {
        1u32 << (256 - u32::from(spc))
    } else {
        u32::from(spc)
    };
    if sectors_per_cluster == 0 || sectors_per_cluster > 1 << 16 {
        return Err(invalid("invalid NTFS cluster size"));
    }
    let bytes_per_cluster = bytes_per_sector * sectors_per_cluster;
    let mft_lcn = u64_at(&boot, 0x30);
    let per_record = boot[0x40] as i8;
    let record_size = if per_record > 0 {
        u32::from(per_record as u8) * bytes_per_cluster
    } else {
        let shift = u32::from(per_record.unsigned_abs());
        if !(9..=16).contains(&shift) {
            return Err(invalid("invalid NTFS record size"));
        }
        1u32 << shift
    };
    if record_size < bytes_per_sector || record_size > 1 << 16 || !record_size.is_power_of_two() {
        return Err(invalid("invalid NTFS record size"));
    }
    let mft_offset = mft_lcn
        .checked_mul(u64::from(bytes_per_cluster))
        .ok_or_else(|| invalid("MFT offset overflow"))?;
    Ok(Geometry {
        bytes_per_sector,
        bytes_per_cluster,
        record_size,
        mft_offset,
    })
}

/// One extent of a non-resident attribute, in clusters. `lcn` is `None` for a sparse hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<u64>,
    pub len: u64,
}

pub fn decode_runs(data: &[u8]) -> io::Result<Vec<Run>> {
    let mut runs = Vec::new();
    let mut pos = 0;
    let mut lcn: i64 = 0;
    while pos < data.len() && data[pos] != 0 {
        let header = data[pos];
        let len_size = usize::from(header & 0x0F);
        let off_size = usize::from(header >> 4);
        pos += 1;
        if len_size == 0 || len_size > 8 || off_size > 8 || pos + len_size + off_size > data.len() {
            return Err(invalid("corrupt NTFS runlist"));
        }
        let mut len = 0u64;
        for (i, &b) in data[pos..pos + len_size].iter().enumerate() {
            len |= u64::from(b) << (8 * i);
        }
        pos += len_size;
        if off_size == 0 {
            runs.push(Run { lcn: None, len });
        } else {
            let mut delta = 0i64;
            for (i, &b) in data[pos..pos + off_size].iter().enumerate() {
                delta |= i64::from(b) << (8 * i);
            }
            if data[pos + off_size - 1] & 0x80 != 0 && off_size < 8 {
                delta |= -1i64 << (8 * off_size);
            }
            pos += off_size;
            lcn = lcn.checked_add(delta).ok_or_else(|| invalid("corrupt NTFS runlist"))?;
            let start = u64::try_from(lcn).map_err(|_| invalid("negative cluster in runlist"))?;
            runs.push(Run { lcn: Some(start), len });
        }
    }
    Ok(runs)
}

/// Reads `len` bytes at byte `offset` of an attribute's value described by `runs`. The
/// underlying reads are cluster aligned, as raw volume handles require. Sparse holes and
/// anything beyond the last run read as zeros.
pub fn read_runs<R: Read + Seek>(reader: &mut R, geometry: &Geometry, runs: &[Run], offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let cluster = u64::from(geometry.bytes_per_cluster);
    let start = offset / cluster * cluster;
    let end = (offset + len as u64).div_ceil(cluster) * cluster;
    let mut buf = vec![0u8; (end - start) as usize];
    let mut pos = 0u64;
    for run in runs {
        let run_bytes = run.len * cluster;
        let from = start.max(pos);
        let to = end.min(pos + run_bytes);
        if from < to
            && let Some(lcn) = run.lcn
        {
            reader.seek(SeekFrom::Start(lcn * cluster + (from - pos)))?;
            reader.read_exact(&mut buf[(from - start) as usize..(to - start) as usize])?;
        }
        pos += run_bytes;
        if pos >= end {
            break;
        }
    }
    buf.drain(..(offset - start) as usize);
    buf.truncate(len);
    Ok(buf)
}

/// Where an `$ATTRIBUTE_LIST` keeps its entries.
#[derive(Debug)]
pub enum AttrList {
    Resident(Vec<u8>),
    NonResident { runs: Vec<Run>, size: u64 },
}

impl AttrList {
    pub fn bytes<R: Read + Seek>(&self, reader: &mut R, geometry: &Geometry) -> io::Result<Vec<u8>> {
        match self {
            AttrList::Resident(bytes) => Ok(bytes.clone()),
            AttrList::NonResident { runs, size } => {
                let size = usize::try_from(*size).map_err(|_| invalid("attribute list too large"))?;
                if size > 64 << 20 {
                    return Err(invalid("attribute list too large"));
                }
                read_runs(reader, geometry, runs, 0, size)
            }
        }
    }
}

/// Distinct extension records holding attributes of `attr_type` (or of any type for `None`),
/// excluding `own`, as listed by an `$ATTRIBUTE_LIST` value.
pub fn list_extension_records(list: &[u8], attr_type: Option<u32>, own: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 26 <= list.len() {
        let rec_len = usize::from(u16_at(list, pos + 4));
        if rec_len < 26 || pos + rec_len > list.len() {
            break;
        }
        let kind = u32_at(list, pos);
        let record = record_number(u64_at(list, pos + 16));
        if attr_type.is_none_or(|t| t == kind) && record != own && !out.contains(&record) {
            out.push(record);
        }
        pos += rec_len;
    }
    out
}

/// A `$DATA` extent segment, kept only for `$MFT`.
#[derive(Debug)]
pub struct DataSegment {
    pub start_vcn: u64,
    /// Data size, meaningful for the segment starting at VCN 0.
    pub size: u64,
    pub runs: Vec<Run>,
}

/// Facts gathered from one MFT record (a base record or an extension record).
#[derive(Debug, Default)]
pub struct Parsed {
    pub number: u64,
    /// Record number of the base record; 0 for a base record.
    pub base: u64,
    pub in_use: bool,
    pub is_dir: bool,
    /// `$FILE_NAME`s other than DOS-only: (parent record number, name).
    pub names: Vec<(u64, String)>,
    /// Modification time, ns since the Unix epoch.
    pub modified: Option<i64>,
    /// Unnamed `$DATA` (logical, on-disk) from the segment that carries sizes.
    pub data: Option<(u64, u64)>,
    pub attr_list: Option<AttrList>,
    pub segments: Vec<DataSegment>,
}

impl Parsed {
    fn merge(&mut self, other: Parsed) {
        self.names.extend(other.names);
        self.modified = self.modified.or(other.modified);
        self.data = self.data.or(other.data);
        self.segments.extend(other.segments);
    }

    /// The finished [`Record`], or `None` when the record is unused or has no name.
    pub fn into_record(self) -> Option<Record> {
        if !self.in_use || self.names.is_empty() {
            return None;
        }
        let (logical, on_disk) = if self.is_dir { (0, 0) } else { self.data.unwrap_or((0, 0)) };
        Some(Record {
            number: self.number,
            is_dir: self.is_dir,
            names: self.names.into_iter().map(|(parent, name)| FileName { parent, name }).collect(),
            modified: self.modified.unwrap_or(0),
            logical,
            on_disk,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileName {
    pub parent: u64,
    pub name: String,
}

/// A file or folder as the MFT describes it, extension records already merged.
#[derive(Clone, Debug)]
pub struct Record {
    pub number: u64,
    pub is_dir: bool,
    /// One per hard link.
    pub names: Vec<FileName>,
    /// ns since the Unix epoch.
    pub modified: i64,
    pub logical: u64,
    pub on_disk: u64,
}

fn apply_fixups(buf: &mut [u8], strict: bool) -> bool {
    let offset = usize::from(u16_at(buf, 4));
    let count = usize::from(u16_at(buf, 6));
    if count == 0 || offset + count * 2 > buf.len() || (count - 1) * FIXUP_STRIDE > buf.len() {
        return !strict;
    }
    let usn = [buf[offset], buf[offset + 1]];
    let ok = (1..count).all(|i| buf[i * FIXUP_STRIDE - 2..i * FIXUP_STRIDE] == usn);
    if ok {
        for i in 1..count {
            let (a, b) = (buf[offset + 2 * i], buf[offset + 2 * i + 1]);
            buf[i * FIXUP_STRIDE - 2] = a;
            buf[i * FIXUP_STRIDE - 1] = b;
        }
    }
    ok || !strict
}

/// Parses one record in place. `strict` rejects records whose update-sequence check fails;
/// the non-strict form also accepts a record whose fixups were already applied by the
/// caller (as `FSCTL_GET_NTFS_FILE_RECORD` may deliver). `keep_runs` retains `$DATA` runlists.
pub fn parse_record(number: u64, buf: &mut [u8], strict: bool, keep_runs: bool) -> Option<Parsed> {
    if buf.len() < 48 || &buf[0..4] != b"FILE" || !apply_fixups(buf, strict) {
        return None;
    }
    let flags = u16_at(buf, 0x16);
    let used = (u32_at(buf, 0x18) as usize).min(buf.len());
    let mut parsed = Parsed {
        number,
        base: record_number(u64_at(buf, 0x20)),
        in_use: flags & FLAG_IN_USE != 0,
        is_dir: flags & FLAG_DIRECTORY != 0,
        ..Parsed::default()
    };
    if !parsed.in_use {
        return Some(parsed);
    }
    let mut pos = usize::from(u16_at(buf, 0x14));
    while pos + 16 <= used {
        let kind = u32_at(buf, pos);
        if kind == ATTR_END {
            break;
        }
        let len = u32_at(buf, pos + 4) as usize;
        if len < 16 || pos + len > used {
            break;
        }
        let attr = &buf[pos..pos + len];
        pos += len;
        let non_resident = attr[8] != 0;
        let named = attr[9] != 0;
        let attr_flags = u16_at(attr, 12);
        if !non_resident {
            if len < 24 {
                continue;
            }
            let value_len = u32_at(attr, 16) as usize;
            let value_off = usize::from(u16_at(attr, 20));
            let Some(value) = attr.get(value_off..value_off + value_len) else {
                continue;
            };
            match kind {
                ATTR_STANDARD_INFORMATION if value.len() >= 16 => {
                    parsed.modified = Some(filetime_to_unix_ns(u64_at(value, 8)));
                }
                ATTR_FILE_NAME if value.len() >= 0x42 => {
                    let chars = usize::from(value[0x40]);
                    let namespace = value[0x41];
                    if namespace != NAMESPACE_DOS
                        && let Some(raw) = value.get(0x42..0x42 + chars * 2)
                    {
                        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                        parsed
                            .names
                            .push((record_number(u64_at(value, 0)), String::from_utf16_lossy(&units)));
                    }
                }
                ATTR_DATA if !named => parsed.data = Some((value_len as u64, 0)),
                ATTR_LIST => parsed.attr_list = Some(AttrList::Resident(value.to_vec())),
                _ => {}
            }
        } else {
            if len < 64 || !matches!(kind, ATTR_DATA | ATTR_LIST) || (kind == ATTR_DATA && named) {
                continue;
            }
            let start_vcn = u64_at(attr, 16);
            let runs_off = usize::from(u16_at(attr, 32));
            let allocated = u64_at(attr, 40);
            let size = u64_at(attr, 48);
            let runs = || attr.get(runs_off..).map(decode_runs).transpose().ok().flatten().unwrap_or_default();
            if kind == ATTR_LIST {
                parsed.attr_list = Some(AttrList::NonResident { runs: runs(), size });
                continue;
            }
            if start_vcn == 0 {
                let packed = attr_flags & (ATTR_COMPRESSED | ATTR_SPARSE) != 0 && runs_off >= 72 && len >= 72;
                let on_disk = if packed { u64_at(attr, 64) } else { allocated };
                parsed.data = Some((size, on_disk));
            }
            if keep_runs {
                parsed.segments.push(DataSegment {
                    start_vcn,
                    size,
                    runs: runs(),
                });
            }
        }
    }
    Some(parsed)
}

fn filetime_to_unix_ns(filetime: u64) -> i64 {
    (filetime.min(i64::MAX as u64) as i64)
        .saturating_sub(FILETIME_UNIX_EPOCH)
        .saturating_mul(100)
}

fn read_record_at<R: Read + Seek>(
    reader: &mut R,
    geometry: &Geometry,
    runs: &[Run],
    number: u64,
    keep_runs: bool,
) -> io::Result<Option<Parsed>> {
    let size = geometry.record_size as usize;
    let mut buf = read_runs(reader, geometry, runs, number * u64::from(geometry.record_size), size)?;
    Ok(parse_record(number, &mut buf, true, keep_runs))
}

/// The extents of `$MFT` and its data size, following its own `$ATTRIBUTE_LIST` if any.
fn mft_extents<R: Read + Seek>(reader: &mut R, geometry: &Geometry) -> io::Result<(Vec<Run>, u64)> {
    let mut buf = vec![0u8; geometry.record_size as usize];
    reader.seek(SeekFrom::Start(geometry.mft_offset))?;
    reader.read_exact(&mut buf)?;
    let mut record0 = parse_record(0, &mut buf, true, true).ok_or_else(|| invalid("$MFT record is corrupt"))?;
    let mut segments = std::mem::take(&mut record0.segments);
    if let Some(list) = &record0.attr_list {
        let bytes = list.bytes(reader, geometry)?;
        let mut known: Vec<Run> = segments.iter().filter(|s| s.start_vcn == 0).flat_map(|s| s.runs.clone()).collect();
        for number in list_extension_records(&bytes, Some(ATTR_DATA), 0) {
            let ext = read_record_at(reader, geometry, &known, number, true)?.ok_or_else(|| invalid("$MFT extension record is corrupt"))?;
            segments.extend(ext.segments);
            known = {
                let mut all: Vec<&DataSegment> = segments.iter().collect();
                all.sort_by_key(|s| s.start_vcn);
                all.iter().flat_map(|s| s.runs.clone()).collect()
            };
        }
    }
    segments.sort_by_key(|s| s.start_vcn);
    let first = segments
        .first()
        .filter(|s| s.start_vcn == 0)
        .ok_or_else(|| invalid("$MFT has no non-resident data"))?;
    let size = first.size;
    Ok((segments.into_iter().flat_map(|s| s.runs).collect(), size))
}

/// Finishes a base record by merging the extension records its `$ATTRIBUTE_LIST` names.
/// `fetch` returns the parsed record with the given number, or `None` if it is unavailable.
/// The result is `None` for an unused, non-base or unnamed record.
pub fn finish_record<R: Read + Seek>(
    mut base: Parsed,
    reader: &mut R,
    geometry: &Geometry,
    mut fetch: impl FnMut(u64) -> io::Result<Option<Parsed>>,
) -> io::Result<Option<Record>> {
    if !base.in_use || base.base != 0 {
        return Ok(None);
    }
    if let Some(list) = base.attr_list.take() {
        let bytes = list.bytes(reader, geometry)?;
        for ext in list_extension_records(&bytes, None, base.number) {
            if let Some(p) = fetch(ext)?
                && p.in_use
                && p.base == base.number
            {
                base.merge(p);
            }
        }
    }
    Ok(base.into_record())
}

/// Reads every in-use record of the volume. Extension records are merged into their bases.
pub fn read_all<R: Read + Seek>(reader: &mut R) -> io::Result<Vec<Record>> {
    let geometry = read_geometry(reader)?;
    let (runs, size) = mft_extents(reader, &geometry)?;
    let record_size = u64::from(geometry.record_size);
    let total = size / record_size;
    let per_chunk = (CHUNK_BYTES / record_size).max(1);

    let mut bases: Vec<Parsed> = Vec::new();
    let mut extensions: Vec<Parsed> = Vec::new();
    let mut first = 0;
    while first < total {
        let count = per_chunk.min(total - first);
        let mut chunk = read_runs(reader, &geometry, &runs, first * record_size, (count * record_size) as usize)?;
        for (i, raw) in chunk.chunks_exact_mut(record_size as usize).enumerate() {
            let Some(parsed) = parse_record(first + i as u64, raw, true, false) else {
                continue;
            };
            if !parsed.in_use {
                continue;
            }
            if parsed.base != 0 {
                extensions.push(parsed)
            } else {
                bases.push(parsed)
            }
        }
        first += count;
    }
    let index: HashMap<u64, usize> = bases.iter().enumerate().map(|(i, p)| (p.number, i)).collect();
    for ext in extensions {
        if let Some(&i) = index.get(&ext.base) {
            bases[i].merge(ext);
        }
    }
    Ok(bases.into_iter().filter_map(Parsed::into_record).collect())
}
