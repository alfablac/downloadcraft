// PAR2 repair — clean-room implementation from public PAR2 spec knowledge only.
// ponytail: scalar GF(2^16) via log/exp tables, no SIMD, no threads. Upgrade: split-table / SSSE3 multiply for large sets.

use md5::{Digest, Md5};
use std::collections::HashMap;
use std::path::Path;

// ---------- CRC32 (IEEE, table-driven) ----------
fn crc32(data: &[u8]) -> u32 {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    let table = T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *v = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

struct FileState {
    desc: FileDesc,
    path: std::path::PathBuf,
    present: bool,
    data: Vec<u8>,
}

fn md5(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    let out = h.finalize();
    let mut a = [0u8; 16];
    a.copy_from_slice(&out);
    a
}

// ---------- GF(2^16): poly 0x1100B, primitive element 2 ----------
const GF_POLY: u32 = 0x1_100B;

fn gf_tables() -> &'static (Vec<u16>, Vec<u16>) {
    // Heap Vecs: 2x64K u16 arrays on stack overflow test threads.
    use std::sync::OnceLock;
    static T: OnceLock<(Vec<u16>, Vec<u16>)> = OnceLock::new();
    T.get_or_init(|| {
        let mut log = vec![0u16; 65536];
        let mut exp = vec![0u16; 65536];
        let mut x: u32 = 1;
        for (i, slot) in exp.iter_mut().enumerate().take(65535) {
            *slot = x as u16;
            log[x as usize] = i as u16;
            x <<= 1;
            if x & 0x10000 != 0 {
                x ^= GF_POLY;
            }
        }
        exp[65535] = 1;
        (log, exp)
    })
}

fn gf_mul(a: u16, b: u16) -> u16 {
    if a == 0 || b == 0 {
        return 0;
    }
    let (log, exp) = gf_tables();
    let s = log[a as usize] as u32 + log[b as usize] as u32;
    exp[(s % 65535) as usize]
}

#[allow(dead_code)] // used by tests / future verify paths
fn gf_div(a: u16, b: u16) -> u16 {
    if a == 0 {
        return 0;
    }
    assert!(b != 0, "gf divide by zero");
    let (log, exp) = gf_tables();
    let s = log[a as usize] as i32 - log[b as usize] as i32;
    exp[s.rem_euclid(65535) as usize]
}

fn gf_pow(a: u16, e: u32) -> u16 {
    if a == 0 {
        return 0;
    }
    if e == 0 {
        return 1;
    }
    let (log, exp) = gf_tables();
    let s = (log[a as usize] as u64 * e as u64) % 65535;
    exp[s as usize]
}

fn gf_inv(a: u16) -> u16 {
    assert!(a != 0, "gf inverse of zero");
    let (log, exp) = gf_tables();
    exp[(65535 - log[a as usize] as u32 % 65535) as usize % 65535]
}

/// Input-slice base generators g_i: 2^{k_i} where k_i runs over positive
/// integers coprime to 65535 (skip divisible by 3,5,17,257), ascending.
/// So k = 1,2,4,7,8,11,13,14,16,... (0-indexed slice order).
fn input_base_logs(count: usize) -> Vec<u16> {
    // Returns log_g values i.e. k_i mod 65535, generator = exp[k_i].
    let mut out = Vec::with_capacity(count);
    let mut k: u32 = 1;
    while out.len() < count {
        if !k.is_multiple_of(3)
            && !k.is_multiple_of(5)
            && !k.is_multiple_of(17)
            && !k.is_multiple_of(257)
        {
            out.push((k % 65535) as u16);
        }
        k += 1;
        if k == 65535 + 1 {
            // Wrapped past field order; PAR2 limits input slices < 65535 anyway.
            break;
        }
    }
    out
}

fn generator_for_base_log(base_log: u16) -> u16 {
    let (_, exp) = gf_tables();
    exp[base_log as usize]
}

// ---------- Packet parsing ----------

const MAGIC: &[u8; 8] = b"PAR2\0PKT";
const TYPE_MAIN: &[u8; 16] = b"PAR 2.0\0Main\0\0\0\0";
const TYPE_FILEDESC: &[u8; 16] = b"PAR 2.0\0FileDesc";
const TYPE_IFSC: &[u8; 16] = b"PAR 2.0\0IFSC\0\0\0\0";
const TYPE_RECV: &[u8; 16] = b"PAR 2.0\0RecvSlic";
const TYPE_CREATOR: &[u8; 16] = b"PAR 2.0\0Creator\0";

fn u64le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}
fn u32le(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[..4]);
    u32::from_le_bytes(a)
}
fn arr16(b: &[u8]) -> [u8; 16] {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[..16]);
    a
}

#[derive(Clone, Debug)]
struct MainPkt {
    slice_size: u64,
    recovery_ids: Vec<[u8; 16]>,
    #[allow(dead_code)]
    non_recovery_ids: Vec<[u8; 16]>,
}

#[derive(Clone, Debug)]
struct FileDesc {
    file_id: [u8; 16],
    hash_full: [u8; 16],
    hash_16k: [u8; 16],
    length: u64,
    name: String,
}

#[derive(Default)]
struct Par2Set {
    #[allow(dead_code)]
    set_id: [u8; 16],
    main: Option<MainPkt>,
    descs: HashMap<[u8; 16], FileDesc>,
    ifsc: HashMap<[u8; 16], Vec<([u8; 16], u32)>>, // per-block (md5, crc32)
    recv: HashMap<u32, Vec<u8>>,                   // exponent -> slice data (first wins)
}

fn parse_packets(data: &[u8], sets: &mut HashMap<[u8; 16], Par2Set>) {
    let mut off = 0usize;
    while off + 64 <= data.len() {
        // Resync to magic if needed.
        if &data[off..off + 8] != MAGIC {
            match data[off..].windows(8).position(|w| w == MAGIC) {
                Some(p) => {
                    off += p;
                    continue;
                }
                None => break,
            }
        }
        if off + 64 > data.len() {
            break;
        }
        let length = u64le(&data[off + 8..off + 16]);
        if length < 64 || !length.is_multiple_of(4) || off as u64 + length > data.len() as u64 {
            // Bad packet: skip past this magic and resync.
            off += 8;
            continue;
        }
        let end = off + length as usize;
        let pkt = &data[off..end];
        // Verify packet MD5: MD5 of bytes [32..end].
        if md5(&pkt[32..]) != arr16(&pkt[16..32]) {
            off = end;
            continue;
        }
        let set_id = arr16(&pkt[32..48]);
        let typ = arr16(&pkt[48..64]);
        let body = &pkt[64..];
        let set = sets.entry(set_id).or_insert_with(|| Par2Set {
            set_id,
            ..Default::default()
        });
        if typ == *TYPE_MAIN {
            if body.len() >= 12 {
                let slice_size = u64le(&body[0..8]);
                let count = u32le(&body[8..12]) as usize;
                let mut rec = Vec::new();
                let mut non = Vec::new();
                let mut p = 12usize;
                for _ in 0..count {
                    if p + 16 <= body.len() {
                        rec.push(arr16(&body[p..p + 16]));
                        p += 16;
                    }
                }
                while p + 16 <= body.len() {
                    non.push(arr16(&body[p..p + 16]));
                    p += 16;
                }
                // Prefer a Main with more recovery ids.
                let better = match &set.main {
                    None => true,
                    Some(m) => rec.len() > m.recovery_ids.len(),
                };
                if better {
                    set.main = Some(MainPkt {
                        slice_size,
                        recovery_ids: rec,
                        non_recovery_ids: non,
                    });
                }
            }
        } else if typ == *TYPE_FILEDESC {
            if body.len() >= 56 {
                let file_id = arr16(&body[0..16]);
                let hash_full = arr16(&body[16..32]);
                let hash_16k = arr16(&body[32..48]);
                let length = u64le(&body[48..56]);
                let raw_name = &body[56..];
                let nul = raw_name
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(raw_name.len());
                let name = String::from_utf8_lossy(&raw_name[..nul]).into_owned();
                set.descs.insert(
                    file_id,
                    FileDesc {
                        file_id,
                        hash_full,
                        hash_16k,
                        length,
                        name,
                    },
                );
            }
        } else if typ == *TYPE_IFSC {
            if body.len() >= 16 {
                let file_id = arr16(&body[0..16]);
                let rest = &body[16..];
                let n = rest.len() / 20;
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push((
                        arr16(&rest[i * 20..i * 20 + 16]),
                        u32le(&rest[i * 20 + 16..i * 20 + 20]),
                    ));
                }
                set.ifsc.entry(file_id).or_insert(v);
            }
        } else if typ == *TYPE_RECV && body.len() >= 4 {
            let exponent = u32le(&body[0..4]);
            set.recv
                .entry(exponent)
                .or_insert_with(|| body[4..].to_vec());
        }
        // Crit / other packets ignored.
        off = end;
    }
}

// ---------- Public API ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    pub files_repaired: usize,
    pub blocks_recovered: usize,
    pub already_ok: bool,
}

fn slices_for_len(len: u64, slice_size: u64) -> usize {
    if slice_size == 0 {
        return 0;
    }
    len.div_ceil(slice_size) as usize
}

/// Verify one block (slice-sized, zero-padded tail handled by caller passing exact slice bytes).
fn block_ok(data: &[u8], want_md5: &[u8; 16], want_crc: u32) -> bool {
    // PAR2 block check: MD5 of slice data AND CRC32 of slice data.
    // Caller passes slice padded to slice_size where required.
    md5(data) == *want_md5 && crc32(data) == want_crc
}

fn md5_16k(data: &[u8]) -> [u8; 16] {
    md5(&data[..data.len().min(16384)])
}

/// Packet exactly like test `build_packet`: length padded to multiple of
/// 4, header MD5 = MD5(bytes[32..end]) (set_id||type||padded body).
fn pkt(set_id: &[u8; 16], typ: &[u8; 16], body: &[u8]) -> Vec<u8> {
    let total = 64 + body.len();
    let padded = total.div_ceil(4) * 4;
    let mut buf = vec![0u8; padded];
    buf[0..8].copy_from_slice(MAGIC);
    buf[8..16].copy_from_slice(&(padded as u64).to_le_bytes());
    buf[32..48].copy_from_slice(set_id);
    buf[48..64].copy_from_slice(typ);
    buf[64..64 + body.len()].copy_from_slice(body);
    let h = md5(&buf[32..padded]);
    buf[16..32].copy_from_slice(&h);
    buf
}

fn slices_of(data: &[u8], slice: u64) -> Vec<Vec<u8>> {
    // Padded slices in order (zero-length file -> one zero slice, matching
    // test build_set `slices_for_len` + repair block layout).
    let s = slice as usize;
    if s == 0 {
        return Vec::new();
    }
    let n = slices_for_len(data.len() as u64, slice);
    let mut out = Vec::with_capacity(n);
    for bi in 0..n {
        let mut v = vec![0u8; s];
        let start = bi * s;
        if start < data.len() {
            let take = (data.len() - start).min(s);
            v[..take].copy_from_slice(&data[start..start + take]);
        }
        out.push(v);
    }
    out
}

fn ifsc(data: &[u8], slice: u64) -> Vec<u8> {
    // Entries: MD5(padded slice) || CRC32(padded slice) LE — repair reads
    // 20-byte entries (md5,crc). No trailing per-slice MD5: parser drops it.
    let mut body = Vec::new();
    for padded in slices_of(data, slice) {
        body.extend_from_slice(&md5(&padded));
        body.extend_from_slice(&crc32(&padded).to_le_bytes());
    }
    body
}

#[doc(hidden)]
pub mod test_support {
    use super::*;
    use std::path::Path;

    /// Build PAR2 set in `dir` from regular files excluding `*.par2`
    /// (sorted by name). Writes `<stem>.par2` (index: Main+FileDesc+IFSC+
    /// Creator, no RecvSlic) and `<stem>.vol000+{recovery:03}.par2`
    /// (critical packets + all RecvSlic). Returns created file names.
    /// `stem` = first sorted file stem, else "set".
    pub fn create_set(
        dir: &Path,
        slice_size: u64,
        recovery_slices: usize,
    ) -> Result<Vec<String>, String> {
        if slice_size == 0 {
            return Err("slice_size 0".into());
        }
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for e in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let p = e.path();
            if !e.file_type().map_err(|e| e.to_string())?.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.to_ascii_lowercase().ends_with(".par2") {
                continue;
            }
            let data = std::fs::read(&p).map_err(|e| e.to_string())?;
            entries.push((name, data));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        if entries.is_empty() {
            return Err("no files in dir".into());
        }
        let mut file_ids: Vec<[u8; 16]> = Vec::new();
        let mut all_slices: Vec<Vec<u8>> = Vec::new();
        for (name, data) in &entries {
            let mut pre = Vec::new();
            pre.extend_from_slice(&md5_16k(data));
            pre.extend_from_slice(&(data.len() as u64).to_le_bytes());
            pre.extend_from_slice(name.as_bytes());
            file_ids.push(md5(&pre));
            all_slices.extend(slices_of(data, slice_size));
        }
        // Repair parser format (parse_packets): Main body =
        // slice(u64) || count(u32) || recovery_ids(count*16) ||
        // non_recovery_ids(rest*16); repair iterates recovery_ids as the
        // file list, so recovery_ids = all file_ids in sorted-name order.
        // Header set_id groups packets; pkt() sets header MD5 correctly.
        let set_id = {
            let mut pre = Vec::new();
            pre.extend_from_slice(&slice_size.to_le_bytes());
            for id in &file_ids {
                pre.extend_from_slice(id);
            }
            let mut sp = Vec::with_capacity(16 + pre.len());
            sp.extend_from_slice(&file_ids[0]);
            sp.extend_from_slice(&pre);
            md5(&sp)
        };
        let mut main_body = Vec::new();
        main_body.extend_from_slice(&slice_size.to_le_bytes());
        main_body.extend_from_slice(&(file_ids.len() as u32).to_le_bytes());
        for id in &file_ids {
            main_body.extend_from_slice(id);
        }
        let packets: Vec<Vec<u8>> = vec![pkt(&set_id, TYPE_MAIN, &main_body)];
        let mut desc_ifsc: Vec<Vec<u8>> = Vec::new();
        for (idx, (name, data)) in entries.iter().enumerate() {
            // Parser FileDesc: file_id(16)||md5_full(16)||md5_16k(16)||
            // len(u64)||name(pad4) — note full hash BEFORE 16k hash.
            let mut desc = Vec::new();
            desc.extend_from_slice(&file_ids[idx]);
            desc.extend_from_slice(&md5(data));
            desc.extend_from_slice(&md5_16k(data));
            desc.extend_from_slice(&(data.len() as u64).to_le_bytes());
            let nb = name.as_bytes();
            desc.extend_from_slice(nb);
            desc.resize(16 + 32 + 8 + ((nb.len() + 3) & !3), 0);
            desc_ifsc.push(pkt(&set_id, b"PAR 2.0\0FileDesc", &desc));
            // IFSC body must start with file_id (parser strips b[0..16]).
            let mut ifsc_body = file_ids[idx].to_vec();
            ifsc_body.extend_from_slice(&ifsc(data, slice_size));
            desc_ifsc.push(pkt(&set_id, TYPE_IFSC, &ifsc_body));
        }
        let creator = pkt(&set_id, TYPE_CREATOR, b"downloadcraft-test\0");
        let stem = {
            let first = &entries[0].0;
            let s = first
                .rsplit_once('.')
                .map(|(a, _)| a)
                .filter(|a| !a.is_empty())
                .unwrap_or(first.as_str());
            s.to_string()
        };
        let index_name = format!("{stem}.par2");
        let vol_name = format!("{stem}.vol000+{recovery_slices:03}.par2");
        let mut index = Vec::new();
        for p in &packets {
            index.extend_from_slice(p);
        }
        for p in &desc_ifsc {
            index.extend_from_slice(p);
        }
        index.extend_from_slice(&creator);
        // Recovery packets: R_e = sum_i g_i^e * D_i, g_i =
        // generator_for_base_log(input_base_logs) per global slice index
        // (same generators repair uses for syndrome/matrix coefficients).
        // Slice order across sorted files; body = exponent(u32)||data.
        let gens: Vec<u16> = input_base_logs(all_slices.len())
            .iter()
            .map(|&k| generator_for_base_log(k))
            .collect();
        let mut vol = index.clone();
        if recovery_slices > 0 && !all_slices.is_empty() {
            for e in 0..recovery_slices {
                let mut acc = vec![0u16; slice_size as usize / 2];
                for (i, d) in all_slices.iter().enumerate() {
                    let coef = gf_pow(gens[i], e as u32);
                    if coef == 0 {
                        continue;
                    }
                    for (j, w) in acc.iter_mut().enumerate() {
                        let lo = d[2 * j] as u16 | ((d[2 * j + 1] as u16) << 8);
                        *w ^= gf_mul(coef, lo);
                    }
                }
                let mut rec_bytes = Vec::with_capacity(acc.len() * 2);
                for w in &acc {
                    rec_bytes.extend_from_slice(&w.to_le_bytes());
                }
                let mut body = Vec::new();
                body.extend_from_slice(&(e as u32).to_le_bytes());
                body.extend_from_slice(&rec_bytes);
                vol.extend_from_slice(&pkt(&set_id, TYPE_RECV, &body));
            }
        }
        std::fs::write(dir.join(&index_name), &index).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(&vol_name), &vol).map_err(|e| e.to_string())?;
        Ok(vec![index_name, vol_name])
    }
}

pub fn repair_dir(dir: &Path) -> Result<RepairReport, String> {
    let mut par2_paths: Vec<std::path::PathBuf> = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension()
            .map(|x| x.eq_ignore_ascii_case("par2"))
            .unwrap_or(false)
        {
            par2_paths.push(p);
        }
    }
    par2_paths.sort();
    if par2_paths.is_empty() {
        return Err("no PAR2 files".to_string());
    }

    let mut sets: HashMap<[u8; 16], Par2Set> = HashMap::new();
    for p in &par2_paths {
        if let Ok(data) = std::fs::read(p) {
            parse_packets(&data, &mut sets);
        }
    }

    // Pick set: has Main, largest recovery-slice count; tie-break most FileDescs.
    let mut best: Option<&Par2Set> = None;
    for s in sets.values() {
        if s.main.is_none() {
            continue;
        }
        let b = match best {
            None => true,
            Some(b) => {
                let bm = b.main.as_ref().unwrap();
                let sm = s.main.as_ref().unwrap();
                s.recv.len() > b.recv.len()
                    || (s.recv.len() == b.recv.len()
                        && sm.recovery_ids.len() > bm.recovery_ids.len())
            }
        };
        if b {
            best = Some(s);
        }
    }
    let set = best.ok_or_else(|| "no PAR2 Main packet found".to_string())?;
    let main = set.main.as_ref().unwrap();
    if main.slice_size == 0 || main.slice_size % 4 != 0 {
        return Err(format!("invalid slice size {}", main.slice_size));
    }
    let slice_size = main.slice_size as usize;

    // Recovery-set files in Main order. Skip verify-only (non-recovery) files.
    let mut files: Vec<FileState> = Vec::new();
    for id in &main.recovery_ids {
        let Some(desc) = set.descs.get(id) else {
            continue;
        };
        // Guard against path traversal in stored names: use file_name only.
        let fname = Path::new(&desc.name)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| desc.name.clone());
        if fname.is_empty() {
            continue;
        }
        let path = dir.join(&fname);
        let (present, data) = match std::fs::read(&path) {
            Ok(d) => (true, d),
            Err(_) => (false, Vec::new()),
        };
        files.push(FileState {
            desc: desc.clone(),
            path,
            present,
            data,
        });
    }
    if files.is_empty() {
        return Err("no recovery-set files described".to_string());
    }

    // Global slice layout.
    let counts: Vec<usize> = files
        .iter()
        .map(|f| slices_for_len(f.desc.length, main.slice_size))
        .collect();
    let total_slices: usize = counts.iter().sum();
    if total_slices == 0 {
        return Ok(RepairReport {
            files_repaired: 0,
            blocks_recovered: 0,
            already_ok: true,
        });
    }
    if total_slices >= 65535 {
        return Err("too many input slices".to_string());
    }
    let base_logs = input_base_logs(total_slices);
    let gens: Vec<u16> = base_logs
        .iter()
        .map(|&k| generator_for_base_log(k))
        .collect();

    // Present slice bytes (padded to slice_size) + missing mask.
    // slice_data[global_idx] = Some(padded bytes) if block verifies, else None.
    let mut slice_data: Vec<Option<Vec<u8>>> = Vec::with_capacity(total_slices);
    let mut file_slice_start: Vec<usize> = Vec::with_capacity(files.len());
    {
        let mut g = 0usize;
        for (fi, f) in files.iter().enumerate() {
            file_slice_start.push(g);
            let checks = set.ifsc.get(&f.desc.file_id);
            for bi in 0..counts[fi] {
                let mut block = vec![0u8; slice_size];
                if f.present {
                    let start = bi * slice_size;
                    if start < f.data.len() {
                        let take = (f.data.len() - start).min(slice_size);
                        block[..take].copy_from_slice(&f.data[start..start + take]);
                    }
                }
                let ok = if !f.present {
                    false
                } else if let Some(chk) = checks.and_then(|c| c.get(bi)) {
                    block_ok(&block, &chk.0, chk.1)
                } else if f.desc.length <= 32768 && bi == 0 {
                    // No IFSC: fall back to full-file / 16k MD5 only for small files, single-slice check not reliable;
                    // treat as missing if full hash mismatches. Handled below per-file; here mark ok if full matches.
                    false
                } else {
                    false
                };
                // Whole-file fast path handled after: if no IFSC, compare hashes.
                slice_data.push(if ok { Some(block) } else { None });
                g += 1;
            }
        }
    }

    // Files without IFSC: verify via FileDesc hashes. If good, restore slices from disk.
    // If bad and file <= slice-covered, slices stay missing (repair via RS).
    for (fi, f) in files.iter().enumerate() {
        if set.ifsc.contains_key(&f.desc.file_id) {
            continue;
        }
        if !f.present {
            continue;
        }
        let full_ok = md5(&f.data) == f.desc.hash_full && f.data.len() as u64 == f.desc.length;
        let k16 = md5(&f.data[..f.data.len().min(16384)]) == f.desc.hash_16k;
        if full_ok && k16 {
            for bi in 0..counts[fi] {
                let mut block = vec![0u8; slice_size];
                let start = bi * slice_size;
                if start < f.data.len() {
                    let take = (f.data.len() - start).min(slice_size);
                    block[..take].copy_from_slice(&f.data[start..start + take]);
                }
                slice_data[file_slice_start[fi] + bi] = Some(block);
            }
        }
    }

    let missing: Vec<usize> = (0..total_slices)
        .filter(|&i| slice_data[i].is_none())
        .collect();
    if missing.is_empty() {
        // Re-verify full hashes for IFSC files too: catch trailing-length issues.
        let mut all_ok = true;
        for f in &files {
            if !f.present
                || md5(&f.data) != f.desc.hash_full
                || f.data.len() as u64 != f.desc.length
            {
                // Block checks passed; length/md5 discrepancy means file needs rewrite from slices.
                // Only flag if we have all slices (we do).
                all_ok = false;
                break;
            }
        }
        if all_ok {
            return Ok(RepairReport {
                files_repaired: 0,
                blocks_recovered: 0,
                already_ok: true,
            });
        }
        // Fall through to rewrite files from verified slices (counts as repair of those files).
        return rewrite_files(&files, &counts, slice_size, &slice_data, 0, true);
    }

    if set.recv.is_empty() {
        return Err(format!(
            "{} blocks missing, no recovery slices",
            missing.len()
        ));
    }
    if missing.len() > set.recv.len() {
        return Err(format!(
            "{} blocks missing, only {} recovery slices available",
            missing.len(),
            set.recv.len()
        ));
    }

    // Use first `missing.len()` recovery slices (lowest exponents for determinism).
    let mut exps: Vec<u32> = set.recv.keys().copied().collect();
    exps.sort_unstable();
    exps.truncate(missing.len());

    // Build syndrome slices: S_e = R_e XOR sum_{present i} g_i^e * D_i
    // Slice data viewed as u16 LE words; GF ops per word.
    let words = slice_size / 2;
    let mut syndromes: Vec<Vec<u16>> = Vec::with_capacity(exps.len());
    for &e in &exps {
        let rdata = &set.recv[&e];
        let mut s = vec![0u16; words];
        for (i, w) in s.iter_mut().enumerate() {
            let lo = rdata.get(i * 2).copied().unwrap_or(0);
            let hi = rdata.get(i * 2 + 1).copied().unwrap_or(0);
            *w = u16::from_le_bytes([lo, hi]);
        }
        // XOR in contributions of present slices.
        for gi in 0..total_slices {
            let Some(d) = &slice_data[gi] else { continue };
            let coeff = gf_pow(gens[gi], e);
            if coeff == 0 {
                continue;
            }
            for i in 0..words {
                let dw = u16::from_le_bytes([d[i * 2], d[i * 2 + 1]]);
                if dw != 0 {
                    s[i] ^= gf_mul(coeff, dw);
                }
            }
        }
        syndromes.push(s);
    }

    // Matrix A[e][m] = g_{missing_m}^e ; solve A * X = S (over GF) via Gauss-Jordan.
    let m = missing.len();
    // Augmented matrix rows: coeffs (m) + syndrome words handled separately.
    // Invert A once (m x m), then apply to each word position.
    let mut mat: Vec<Vec<u16>> = vec![vec![0u16; 2 * m]; m];
    for (ei, &e) in exps.iter().enumerate() {
        for (mi, &gi) in missing.iter().enumerate() {
            mat[ei][mi] = gf_pow(gens[gi], e);
        }
        mat[ei][m + ei] = 1;
    }
    // Gauss-Jordan.
    for col in 0..m {
        // Pivot.
        let piv = (col..m)
            .find(|&r| mat[r][col] != 0)
            .ok_or("singular recovery matrix")?;
        mat.swap(col, piv);
        let inv = gf_inv(mat[col][col]);
        // ponytail: index loops kept; row split-borrow blocks iter_mut rewrite
        #[allow(clippy::needless_range_loop)]
        {
            for j in 0..2 * m {
                mat[col][j] = gf_mul(mat[col][j], inv);
            }
            for r in 0..m {
                if r != col && mat[r][col] != 0 {
                    let f = mat[r][col];
                    for j in 0..2 * m {
                        let v = gf_mul(f, mat[col][j]);
                        mat[r][j] ^= v;
                    }
                }
            }
        }
    }
    // mat[*][m..] = A_inv rows.
    // Recovered slice m-th: X_m = sum_e A_inv[m][e] * S_e  (per word)
    let mut recovered: HashMap<usize, Vec<u8>> = HashMap::new();
    for (mi, &gi) in missing.iter().enumerate() {
        let mut out = vec![0u8; slice_size];
        for i in 0..words {
            let mut acc: u16 = 0;
            for ei in 0..m {
                let c = mat[mi][m + ei];
                if c != 0 && syndromes[ei][i] != 0 {
                    acc ^= gf_mul(c, syndromes[ei][i]);
                }
            }
            out[i * 2..i * 2 + 2].copy_from_slice(&acc.to_le_bytes());
        }
        recovered.insert(gi, out);
    }

    // Fill slice_data with recovered.
    for (gi, bytes) in &recovered {
        slice_data[*gi] = Some(bytes.clone());
    }

    // Verify recovered blocks against IFSC (when available), then rewrite files.
    for &gi in &missing {
        // Locate file/block.
        let mut acc = 0usize;
        for fi in 0..files.len() {
            if gi < acc + counts[fi] {
                let bi = gi - acc;
                if let Some(chk) = set
                    .ifsc
                    .get(&files[fi].desc.file_id)
                    .and_then(|c| c.get(bi))
                {
                    let d = slice_data[gi].as_ref().unwrap();
                    if !block_ok(d, &chk.0, chk.1) {
                        return Err(format!(
                            "recovered block {} in {} failed MD5/CRC32 verify",
                            bi, files[fi].desc.name
                        ));
                    }
                }
                break;
            }
            acc += counts[fi];
        }
    }

    rewrite_files(
        &files,
        &counts,
        slice_size,
        &slice_data,
        missing.len(),
        true,
    )
}

#[allow(clippy::too_many_arguments)]
// ---------- Test support: packet generation ----------
#[cfg(test)]
fn build_packet(set_id: &[u8; 16], typ: &[u8; 16], body: &[u8]) -> Vec<u8> {
    let total = 64 + body.len();
    let padded = total.div_ceil(4) * 4;
    let mut buf = vec![0u8; padded];
    buf[0..8].copy_from_slice(MAGIC);
    buf[8..16].copy_from_slice(&(padded as u64).to_le_bytes());
    // MD5 filled after body placed: MD5 over bytes [32..padded].
    buf[32..48].copy_from_slice(set_id);
    buf[48..64].copy_from_slice(typ);
    buf[64..64 + body.len()].copy_from_slice(body);
    let h = md5(&buf[32..padded]);
    buf[16..32].copy_from_slice(&h);
    buf
}

#[cfg(test)]
fn compute_recv_slice(
    slices: &[Vec<u8>],
    gens: &[u16],
    exponent: u32,
    slice_size: usize,
) -> Vec<u8> {
    // R_e = sum_i g_i^e * D_i over GF words.
    let words = slice_size / 2;
    let mut acc = vec![0u16; words];
    for (i, d) in slices.iter().enumerate() {
        let c = gf_pow(gens[i], exponent);
        if c == 0 {
            continue;
        }
        for w in 0..words {
            let dw = u16::from_le_bytes([d[w * 2], d[w * 2 + 1]]);
            if dw != 0 {
                acc[w] ^= gf_mul(c, dw);
            }
        }
    }
    let mut out = vec![0u8; slice_size];
    for (w, v) in acc.iter().enumerate() {
        out[w * 2..w * 2 + 2].copy_from_slice(&v.to_le_bytes());
    }
    out
}

fn rewrite_files(
    files: &[FileState],
    counts: &[usize],
    slice_size: usize,
    slice_data: &[Option<Vec<u8>>],
    blocks_recovered: usize,
    only_changed: bool,
) -> Result<RepairReport, String> {
    // Rebuild each file from slices, truncate to desc length, write if changed/missing.
    // Then re-verify full MD5 + 16k MD5; count files actually rewritten.
    let mut files_repaired = 0usize;
    let mut g = 0usize;
    for (fi, f) in files.iter().enumerate() {
        let mut rebuilt: Vec<u8> = Vec::with_capacity(f.desc.length as usize);
        for bi in 0..counts[fi] {
            let sd = slice_data[g + bi]
                .as_ref()
                .ok_or_else(|| format!("slice {} unavailable", g + bi))?;
            let want = ((f.desc.length as usize).saturating_sub(bi * slice_size)).min(slice_size);
            rebuilt.extend_from_slice(&sd[..want]);
        }
        g += counts[fi];
        if rebuilt.len() as u64 != f.desc.length {
            return Err(format!("rebuilt length mismatch for {}", f.desc.name));
        }
        // Verify before writing.
        if md5(&rebuilt) != f.desc.hash_full {
            return Err(format!(
                "rebuilt file {} failed full MD5 verify",
                f.desc.name
            ));
        }
        if md5(&rebuilt[..rebuilt.len().min(16384)]) != f.desc.hash_16k {
            return Err(format!(
                "rebuilt file {} failed 16k MD5 verify",
                f.desc.name
            ));
        }
        let changed = !f.present || f.data != rebuilt;
        if changed || !only_changed {
            // Write atomically-ish: write then rename over target.
            let tmp = f.path.with_extension("par2tmp");
            std::fs::write(&tmp, &rebuilt).map_err(|e| format!("write {}: {e}", tmp.display()))?;
            // Windows: rename over existing fails; remove first.
            let _ = std::fs::remove_file(&f.path);
            std::fs::rename(&tmp, &f.path)
                .map_err(|e| format!("rename {}: {e}", f.path.display()))?;
            files_repaired += 1;
        }
        // Re-verify on disk.
        let disk =
            std::fs::read(&f.path).map_err(|e| format!("reread {}: {e}", f.path.display()))?;
        if md5(&disk) != f.desc.hash_full {
            return Err(format!("post-write MD5 verify failed for {}", f.desc.name));
        }
    }
    Ok(RepairReport {
        files_repaired,
        blocks_recovered,
        already_ok: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn gf_sanity() {
        // 2 * inv(2) == 1; sample a*inv(a)==1
        assert_eq!(gf_mul(2, gf_inv(2)), 1);
        for a in [1u16, 2, 3, 0x1234, 0xFFFE, 0x8000, 42] {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "a={a:#x}");
            assert_eq!(gf_div(a, a), 1);
            assert_eq!(gf_mul(a, 1), a);
            assert_eq!(gf_mul(a, 0), 0);
        }
        // primitive: 2 is generator -> pow cycle 65535
        assert_eq!(gf_pow(2, 65535), 1);
        assert_eq!(gf_pow(2, 1), 2);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn input_bases_first_values() {
        let logs = input_base_logs(8);
        assert_eq!(logs, vec![1, 2, 4, 7, 8, 11, 13, 14]);
        let (_, exp) = gf_tables();
        let gens: Vec<u16> = logs.iter().map(|&k| exp[k as usize]).collect();
        assert_eq!(gens[0], 2); // 2^1
        assert_eq!(gens[1], 4); // 2^2
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("webcraft-par2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // Build a full set: 2 files, slice 16, recv exponents 0..nrecv. Returns par2 bytes.
    fn build_set(files_data: &[(&str, Vec<u8>)], slice_size: usize, nrecv: u32) -> Vec<u8> {
        let set_id: [u8; 16] = md5(b"test-set");
        // FileDesc needs file_id: spec derives from hash16k+length+name; tests use stable fake ids.
        let mut descs: Vec<FileDesc> = Vec::new();
        let mut slices: Vec<Vec<u8>> = Vec::new();
        let mut per_file_blocks: Vec<Vec<([u8; 16], u32)>> = Vec::new();
        for (name, data) in files_data {
            let hash_full = md5(data);
            let hash_16k = md5(&data[..data.len().min(16384)]);
            let mut id_src = Vec::new();
            id_src.extend_from_slice(&hash_16k);
            id_src.extend_from_slice(&(data.len() as u64).to_le_bytes());
            id_src.extend_from_slice(name.as_bytes());
            let file_id = md5(&id_src);
            let n = slices_for_len(data.len() as u64, slice_size as u64);
            let mut blocks = Vec::new();
            for bi in 0..n {
                let mut block = vec![0u8; slice_size];
                let s = bi * slice_size;
                let take = (data.len() - s).min(slice_size);
                block[..take].copy_from_slice(&data[s..s + take]);
                blocks.push((md5(&block), crc32(&block)));
                slices.push(block);
            }
            per_file_blocks.push(blocks);
            descs.push(FileDesc {
                file_id,
                hash_full,
                hash_16k,
                length: data.len() as u64,
                name: name.to_string(),
            });
        }
        let total = slices.len();
        let base_logs = input_base_logs(total);
        let gens: Vec<u16> = base_logs
            .iter()
            .map(|&k| generator_for_base_log(k))
            .collect();
        let mut out: Vec<u8> = Vec::new();
        // Main
        {
            let mut body = Vec::new();
            body.extend_from_slice(&(slice_size as u64).to_le_bytes());
            body.extend_from_slice(&(descs.len() as u32).to_le_bytes());
            for d in &descs {
                body.extend_from_slice(&d.file_id);
            }
            out.extend_from_slice(&build_packet(&set_id, TYPE_MAIN, &body));
        }
        for (i, d) in descs.iter().enumerate() {
            let mut body = Vec::new();
            body.extend_from_slice(&d.file_id);
            body.extend_from_slice(&d.hash_full);
            body.extend_from_slice(&d.hash_16k);
            body.extend_from_slice(&d.length.to_le_bytes());
            body.extend_from_slice(d.name.as_bytes());
            out.extend_from_slice(&build_packet(&set_id, TYPE_FILEDESC, &body));
            let mut body2 = Vec::new();
            body2.extend_from_slice(&d.file_id);
            for (m, c) in &per_file_blocks[i] {
                body2.extend_from_slice(m);
                body2.extend_from_slice(&c.to_le_bytes());
            }
            out.extend_from_slice(&build_packet(&set_id, TYPE_IFSC, &body2));
        }
        for e in 0..nrecv {
            let r = compute_recv_slice(&slices, &gens, e, slice_size);
            let mut body = Vec::new();
            body.extend_from_slice(&e.to_le_bytes());
            body.extend_from_slice(&r);
            out.extend_from_slice(&build_packet(&set_id, TYPE_RECV, &body));
        }
        out
    }

    #[test]
    fn packet_roundtrip_and_repair_flip() {
        let dir = tempdir("flip");
        let f1: Vec<u8> = (0..40u8).collect(); // 3 slices @16
        let f2: Vec<u8> = (0..33u8)
            .map(|x| x.wrapping_mul(7).wrapping_add(3))
            .collect(); // 3 slices, tail padded
        std::fs::write(dir.join("a.bin"), &f1).unwrap();
        std::fs::write(dir.join("b.bin"), &f2).unwrap();
        let par2 = build_set(&[("a.bin", f1.clone()), ("b.bin", f2.clone())], 16, 3);
        std::fs::write(dir.join("x.par2"), &par2).unwrap();

        // Packet parse roundtrip check
        let mut sets: HashMap<[u8; 16], Par2Set> = HashMap::new();
        parse_packets(&par2, &mut sets);
        assert_eq!(sets.len(), 1);
        let s = sets.values().next().unwrap();
        assert_eq!(s.main.as_ref().unwrap().slice_size, 16);
        assert_eq!(s.descs.len(), 2);
        assert_eq!(s.recv.len(), 3);

        // Clean dir: already_ok
        let r = repair_dir(&dir).unwrap();
        assert!(r.already_ok, "{r:?}");
        assert_eq!(r.files_repaired, 0);

        // Flip bytes inside first slice of a.bin (block 0 corrupt, still same length)
        let mut bad = f1.clone();
        bad[0] ^= 0xFF;
        bad[3] ^= 0xFF;
        std::fs::write(dir.join("a.bin"), &bad).unwrap();
        let r = repair_dir(&dir).unwrap();
        assert!(!r.already_ok);
        assert_eq!(r.blocks_recovered, 1);
        assert_eq!(std::fs::read(dir.join("a.bin")).unwrap(), f1);
        assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), f2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repair_two_missing_blocks_and_missing_file() {
        let dir = tempdir("two");
        let f1: Vec<u8> = (0..40u8).map(|x| x ^ 0x5A).collect();
        let f2: Vec<u8> = (0..20u8).map(|x| x.wrapping_mul(3)).collect();
        std::fs::write(dir.join("a.bin"), &f1).unwrap();
        std::fs::write(dir.join("b.bin"), &f2).unwrap();
        let par2 = build_set(&[("a.bin", f1.clone()), ("b.bin", f2.clone())], 16, 4);
        std::fs::write(dir.join("x.par2"), &par2).unwrap();
        // Corrupt 1 block in a.bin, delete b.bin (2 blocks) => 3 missing, 4 recv available
        let mut bad = f1.clone();
        bad[20] ^= 0xFF; // slice 1
        std::fs::write(dir.join("a.bin"), &bad).unwrap();
        std::fs::remove_file(dir.join("b.bin")).unwrap();
        let r = repair_dir(&dir).unwrap();
        assert_eq!(r.blocks_recovered, 3, "{r:?}");
        assert_eq!(std::fs::read(dir.join("a.bin")).unwrap(), f1);
        assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), f2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_set_roundtrip_repairs_hole() {
        let dir = tempdir("createset");
        let data: Vec<u8> = (0u32..200).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.join("movie.bin"), &data).unwrap();
        let names = crate::par2::test_support::create_set(&dir, 64, 2).unwrap();
        assert_eq!(names.len(), 2);
        assert!(dir.join(&names[0]).exists() && dir.join(&names[1]).exists());
        let mut damaged = data.clone();
        for b in &mut damaged[64..128] {
            *b = 0;
        }
        std::fs::write(dir.join("movie.bin"), &damaged).unwrap();
        let r = repair_dir(&dir).unwrap();
        assert!(r.files_repaired >= 1, "{r:?}");
        assert_eq!(std::fs::read(dir.join("movie.bin")).unwrap(), data);
        // Two damaged slices: forces exponent-1 recovery slice + matrix
        // solve (e=0 alone is underdetermined for 2 unknowns).
        let mut damaged2 = data.clone();
        for b in &mut damaged2[0..64] {
            *b ^= 0xFF;
        }
        for b in &mut damaged2[128..192] {
            *b = 0;
        }
        std::fs::write(dir.join("movie.bin"), &damaged2).unwrap();
        let r = repair_dir(&dir).unwrap();
        assert_eq!(r.blocks_recovered, 2, "{r:?}");
        assert_eq!(std::fs::read(dir.join("movie.bin")).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_par2_files_err() {
        let dir = tempdir("empty");
        let e = repair_dir(&dir).unwrap_err();
        assert_eq!(e, "no PAR2 files");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
