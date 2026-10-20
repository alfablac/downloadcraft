// Auto-unpack for completed NZB downloads: archives extracted via staging
// dir inside out dir, entries moved in place only after every entry was
// verified (CRC-checked by backend). Archive files deleted only after that.
// Any failure removes staging, leaves archive + pre-existing files untouched.
use std::path::{Component, Path, PathBuf};

/// Hard cap on total extracted bytes per archive (zip-bomb guard).
const MAX_EXTRACTED: u64 = 100 * 1024 * 1024 * 1024;

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// True for first-volume / standalone archive names NZB unpack handles.
/// Subsequent volumes (`.part2.rar`, `.r00`, `.7z.002`, `.z01`) excluded:
/// backend opens them via first volume.
pub fn is_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // Subsequent volumes excluded: backend opens them via first volume.
    if part_number(&lower).is_some_and(|n| n != 1) {
        return false;
    }
    if numeric_volume(&lower).is_some_and(|n| n > 1) {
        return false;
    }
    matches!(ext_of(&lower).as_str(), "rar" | "zip" | "7z")
        || lower.ends_with(".7z.001")
        || lower.ends_with(".zip.001")
}

/// `.part12.rar` volume number, else None.
fn part_number(lower: &str) -> Option<u32> {
    let idx = lower.rfind(".part")?;
    let tail = &lower[idx + 5..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && tail[digits.len()..].starts_with(".rar") {
        return digits.parse().ok();
    }
    None
}

/// Trailing `.001`/`.002` split-volume number (7z/zip split sets).
fn numeric_volume(lower: &str) -> Option<u32> {
    let ext = lower.rsplit_once('.')?.1;
    if ext.len() == 3 && ext.chars().all(|c| c.is_ascii_digit()) {
        return ext.parse().ok();
    }
    // `.7z.001` style
    if let Some(idx) = lower.rfind('.') {
        let tail = &lower[idx + 1..];
        if tail.len() == 3 && tail.chars().all(|c| c.is_ascii_digit()) {
            return tail.parse().ok();
        }
    }
    None
}

/// All volume files of archive `path` present in `dir` (for delete on
/// success). Single-volume archives return just themselves.
pub fn archive_parts(dir: &Path, name: &str) -> Vec<PathBuf> {
    let lower = name.to_ascii_lowercase();
    let mut parts = vec![dir.join(name)];
    let read_names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    if let Some(idx) = lower.rfind(".part") {
        // new-style .partN.rar: same stem, numeric N only.
        let stem = &lower[..idx];
        for n in &read_names {
            let ln = n.to_ascii_lowercase();
            if ln != lower && part_number(&ln).is_some() {
                if let Some(pidx) = ln.rfind(".part") {
                    if &ln[..pidx] == stem {
                        parts.push(dir.join(n));
                    }
                }
            }
        }
    } else if lower.ends_with(".rar") {
        // old-style: .r00, .r01, ... next to .rar
        let stem = &lower[..lower.len() - 4];
        for n in &read_names {
            let ln = n.to_ascii_lowercase();
            if let Some(ext) = ln.strip_prefix(&format!("{stem}.")) {
                if ext.len() == 3
                    && ext.starts_with('r')
                    && ext[1..].chars().all(|c| c.is_ascii_digit())
                {
                    parts.push(dir.join(n));
                }
            }
        }
    } else if lower.ends_with(".7z.001") || numeric_volume(&lower) == Some(1) {
        // split .001/.002...: stem before final .NNN
        if let Some(idx) = lower.rfind('.') {
            let stem = &lower[..idx];
            for n in &read_names {
                let ln = n.to_ascii_lowercase();
                if ln != lower {
                    if let Some(rest) = ln.strip_prefix(&format!("{stem}.")) {
                        if rest.len() == 3 && rest.chars().all(|c| c.is_ascii_digit()) {
                            parts.push(dir.join(n));
                        }
                    }
                }
            }
        }
    }
    parts.sort();
    parts.dedup();
    parts
}

/// Reject absolute / parent-escaping entry names. Returns safe relative path.
fn enclosed(name: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in name.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn fmt_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Extract one archive into `dir` via staging subdir. Err = archive kept,
/// `dir` pre-existing files untouched.
/// ponytail: RAR entries read whole into RAM (unrar read() has no streaming);
/// NZB releases fine, add extract_with_base two-pass if GB entries matter.
pub fn extract_archive(path: &Path, dir: &Path, password: Option<&str>) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging = dir.join(format!(
        ".unpack-tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&staging).map_err(fmt_err)?;
    let res = extract_into(path, &staging, password)
        .and_then(|()| {
            // Catches formats without own accounting (7z): cap staged bytes.
            fn sum(p: &Path) -> u64 {
                std::fs::read_dir(p)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| {
                                let path = e.path();
                                if path.is_dir() {
                                    sum(&path)
                                } else {
                                    e.metadata().map(|m| m.len()).unwrap_or(0)
                                }
                            })
                            .sum()
                    })
                    .unwrap_or(0)
            }
            if sum(&staging) > MAX_EXTRACTED {
                return Err("extracted size exceeds cap".to_string());
            }
            Ok(())
        })
        .and_then(|()| move_staged(&staging, dir));
    let _ = std::fs::remove_dir_all(&staging);
    res.map_err(|e| format!("{e} (archive {name} kept)"))
}

fn extract_into(path: &Path, dir: &Path, password: Option<&str>) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".zip.001") {
        extract_split_zip(path, dir, password)
    } else if lower.ends_with(".zip") {
        extract_zip(path, dir, password)
    } else if lower.ends_with(".7z") || lower.ends_with(".7z.001") {
        extract_7z(path, dir, password)
    } else if lower.ends_with(".rar") {
        extract_rar(path, dir, password)
    } else {
        Err(format!("unsupported archive: {name}"))
    }
}

/// Move staged entries into `dir`. Pre-validates type conflicts, then renames
/// (staging lives inside dir: same filesystem, rename never truncates first).
/// Per-entry rename atomic; cross-entry atomicity not guaranteed on OS error.
fn move_staged(staging: &Path, dir: &Path) -> Result<(), String> {
    fn check(src: &Path, dst: &Path) -> Result<(), String> {
        for e in std::fs::read_dir(src).map_err(fmt_err)? {
            let e = e.map_err(fmt_err)?;
            let to = dst.join(e.file_name());
            let is_dir = e.file_type().map_err(fmt_err)?.is_dir();
            if let Ok(m) = std::fs::metadata(&to) {
                if m.is_dir() != is_dir {
                    return Err(format!("unpack target type conflict: {}", to.display()));
                }
            }
            if is_dir && to.is_dir() {
                check(&e.path(), &to)?;
            }
        }
        Ok(())
    }
    fn walk(src: &Path, dst: &Path) -> Result<(), String> {
        for e in std::fs::read_dir(src).map_err(fmt_err)? {
            let e = e.map_err(fmt_err)?;
            let to = dst.join(e.file_name());
            if e.file_type().map_err(fmt_err)?.is_dir() {
                std::fs::create_dir_all(&to).map_err(fmt_err)?;
                walk(&e.path(), &to)?;
            } else {
                std::fs::rename(e.path(), &to).map_err(fmt_err)?;
            }
        }
        Ok(())
    }
    check(staging, dir)?;
    walk(staging, dir)
}

fn extract_zip(path: &Path, dir: &Path, password: Option<&str>) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(fmt_err)?;
    let mut za = zip::ZipArchive::new(file).map_err(fmt_err)?;
    if za
        .decompressed_size()
        .is_some_and(|s| s > MAX_EXTRACTED as u128)
    {
        return Err("zip extracted size exceeds cap".to_string());
    }
    let mut total_written: u64 = 0;
    for i in 0..za.len() {
        let mut entry = match password {
            Some(pw) if !pw.is_empty() => za
                .by_index_decrypt(i, pw.as_bytes())
                .map_err(fmt_err)?,
            _ => za.by_index(i).map_err(fmt_err)?,
        };
        let Some(rel) = entry.enclosed_name() else {
            return Err("unsafe path in zip entry".to_string());
        };
        let dest = dir.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest).map_err(fmt_err)?;
            continue;
        }
        // Symlink entries: never materialize link target; store as plain file would escape on later use. Skip link, keep payload out.
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err("symlink entry not extracted".to_string());
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(fmt_err)?;
        }
        let mut out = std::fs::File::create(&dest).map_err(fmt_err)?;
        // Counted copy: cap applies to bytes actually written (declared sizes
        // can lie / be absent with data descriptors). CRC verified at EOF.
        let mut written: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            use std::io::Read as _;
            let n = match entry.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    drop(out);
                    let _ = std::fs::remove_file(&dest);
                    return Err(fmt_err(e));
                }
            };
            written = written.saturating_add(n as u64);
            if total_written.saturating_add(written) > MAX_EXTRACTED {
                drop(out);
                let _ = std::fs::remove_file(&dest);
                return Err("zip extracted size exceeds cap".to_string());
            }
            if let Err(e) = std::io::Write::write_all(&mut out, &buf[..n]) {
                drop(out);
                let _ = std::fs::remove_file(&dest);
                return Err(fmt_err(e));
            }
        }
        total_written = total_written.saturating_add(written);
    }
    Ok(())
}

/// Byte-split `.zip.001+.002...`: concat into staging-local file, then zip pass.
fn extract_split_zip(path: &Path, dir: &Path, password: Option<&str>) -> Result<(), String> {
    let orig_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(dir);
    let parts = archive_parts(parent, &orig_name);
    if parts.len() <= 1 {
        return extract_zip(path, dir, password);
    }
    let tmp = dir.join(".concat.zip");
    let res = (|| -> Result<(), String> {
        let mut out = std::fs::File::create(&tmp).map_err(fmt_err)?;
        for p in &parts {
            let mut f = std::fs::File::open(p).map_err(fmt_err)?;
            std::io::copy(&mut f, &mut out).map_err(fmt_err)?;
        }
        drop(out);
        extract_zip(&tmp, dir, password)
    })();
    let _ = std::fs::remove_file(&tmp);
    res
}

fn extract_7z(path: &Path, dir: &Path, password: Option<&str>) -> Result<(), String> {
    let pw = match password {
        Some(p) if !p.is_empty() => sevenz_rust2::Password::from(p),
        _ => sevenz_rust2::Password::empty(),
    };
    // Split .001 sets are byte-split: concat volumes into staging-local temp.
    // NOTE: dir here is staging dir, volumes live in parent (real out dir).
    let orig_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if orig_name.to_ascii_lowercase().ends_with(".001") {
        let parent = path.parent().unwrap_or(dir);
        let parts = archive_parts(parent, &orig_name);
        if parts.len() > 1 {
            let tmp = dir.join(".concat.7z");
            let res = (|| -> Result<(), String> {
                let mut out = std::fs::File::create(&tmp).map_err(fmt_err)?;
                for p in &parts {
                    let mut f = std::fs::File::open(p).map_err(fmt_err)?;
                    std::io::copy(&mut f, &mut out).map_err(fmt_err)?;
                }
                drop(out);
                sevenz_rust2::decompress_with_password(
                    std::fs::File::open(&tmp).map_err(fmt_err)?,
                    dir,
                    pw.clone(),
                )
                .map_err(fmt_err)
            })();
            let _ = std::fs::remove_file(&tmp);
            return res;
        }
    }
    sevenz_rust2::decompress_with_password(
        std::fs::File::open(path).map_err(fmt_err)?,
        dir,
        pw,
    )
    .map_err(fmt_err)
}

fn extract_rar(path: &Path, dir: &Path, password: Option<&str>) -> Result<(), String> {
    let pw: &[u8] = password.unwrap_or("").as_bytes();
    let first = if pw.is_empty() {
        unrar_ng::Archive::new(path).open_for_processing()
    } else {
        unrar_ng::Archive::with_password(path, &pw).open_for_processing()
    }
    .map_err(fmt_err)?;
    let mut arc = Some(first);
    let mut total: u64 = 0;
    loop {
        let cur = arc.take().unwrap();
        let Some(before) = cur.read_header().map_err(fmt_err)? else {
            break;
        };
        let (is_dir, filename, usize_) = {
            let hdr = before.entry();
            (hdr.is_directory(), hdr.filename.clone(), hdr.unpacked_size)
        };
        total = total.saturating_add(usize_);
        if total > MAX_EXTRACTED {
            return Err("rar extracted size exceeds cap".to_string());
        }
        // read() buffers whole entry; refuse single entries >4 GiB.
        if usize_ > 4 * 1024 * 1024 * 1024 {
            return Err("rar entry exceeds 4 GiB in-memory cap".to_string());
        }
        if is_dir {
            arc = Some(before.skip().map_err(fmt_err)?);
            continue;
        }
        let Some(rel) = enclosed(&filename) else {
            return Err(format!("unsafe path in rar: {}", filename.display()));
        };
        let (data, rest) = before.read().map_err(fmt_err)?;
        let dest = dir.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(fmt_err)?;
        }
        std::fs::write(&dest, &data).map_err(fmt_err)?;
        arc = Some(rest);
    }
    Ok(())
}

/// Unpack every archive in `dir` (names from NZB file list). Archives deleted
/// only after full verified extraction. Returns count unpacked.
/// Failure of any archive aborts with Err; earlier archives in same NZB stay
/// unpacked+deleted (their extraction already verified).
pub fn unpack_dir(dir: &Path, names: &[String], password: Option<&str>) -> Result<usize, String> {
    let mut done = 0usize;
    for name in names {
        if !is_archive(name) {
            continue;
        }
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        // ponytail: no nested-archive recursion; extracted .rar stays for manual unpack.
        extract_archive(&path, dir, password)
            .map_err(|e| format!("unpack {}: {e}", name))?;
        for part in archive_parts(dir, name) {
            std::fs::remove_file(&part)
                .map_err(|e| format!("extracted OK but could not delete {}: {e}", part.display()))?;
        }
        done += 1;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_detection() {
        assert!(is_archive("movie.rar"));
        assert!(is_archive("movie.zip"));
        assert!(is_archive("movie.7z"));
        assert!(is_archive("movie.part1.rar"));
        assert!(is_archive("movie.part01.rar"));
        assert!(!is_archive("movie.part2.rar"));
        assert!(!is_archive("movie.r00"));
        assert!(!is_archive("movie.mkv"));
        assert!(!is_archive("movie.7z.002"));
        assert!(is_archive("movie.7z.001"));
    }

    #[test]
    fn enclosed_rejects_escape() {
        assert_eq!(enclosed(Path::new("a/b.txt")), Some(PathBuf::from("a/b.txt")));
        assert_eq!(enclosed(Path::new("../evil")), None);
        assert_eq!(enclosed(Path::new("/abs")), None);
    }

    #[test]
    fn stored_zip_extract_and_corrupt() {
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Stored zip via zip crate writer (test through public extract fn).
        let zip_path = dir.join("pack.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("hello.txt", opts).unwrap();
            std::io::Write::write_all(&mut w, b"hello unpack").unwrap();
            w.finish().unwrap();
        }
        let n = unpack_dir(&dir, &["pack.zip".to_string()], None).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(dir.join("hello.txt")).unwrap(), b"hello unpack");
        assert!(!zip_path.exists(), "archive deleted on success");

        // Corrupt: flip payload byte, CRC must fail, archive kept.
        let bad_path = dir.join("bad.zip");
        {
            let f = std::fs::File::create(&bad_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("x.txt", opts).unwrap();
            std::io::Write::write_all(&mut w, b"payload-data").unwrap();
            w.finish().unwrap();
        }
        let mut bytes = std::fs::read(&bad_path).unwrap();
        let pos = bytes.windows(12).position(|w| w == b"payload-data").unwrap();
        bytes[pos] ^= 0xFF;
        std::fs::write(&bad_path, &bytes).unwrap();
        let err = unpack_dir(&dir, &["bad.zip".to_string()], None).unwrap_err();
        assert!(err.contains("unpack bad.zip"), "{err}");
        assert!(bad_path.exists(), "archive kept on failure");
        assert!(!dir.join("x.txt").exists(), "no partial entry left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sevenz_extract_and_delete() {
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-7z-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"seven zip payload").unwrap();
        let arc = dir.join("pack.7z");
        sevenz_rust2::compress_to_path(&src, &arc).unwrap();
        let n = unpack_dir(&dir, &["pack.7z".to_string()], None).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"seven zip payload");
        assert!(!arc.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal RAR5 Stored archive hand-crafted (no RAR encoder available/allowed).
    fn rar5_stored(name: &str, data: &[u8]) -> Vec<u8> {
        fn vint(out: &mut Vec<u8>, mut v: u64) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v > 0 {
                    b |= 0x80;
                }
                out.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        fn header(out: &mut Vec<u8>, body: &[u8]) {
            let mut inner = Vec::new();
            vint(&mut inner, body.len() as u64);
            inner.extend_from_slice(body);
            let crc = crate::nntp::crc32(&inner);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&inner);
        }
        let mut out = b"Rar!\x1a\x07\x01\x00".to_vec();
        // main head: type 1, flags 0, archive flags 0
        let mut main = Vec::new();
        vint(&mut main, 1);
        vint(&mut main, 0);
        vint(&mut main, 0);
        header(&mut out, &main);
        // file head: type 2, hflags 0x02 (data area present)
        let mut file = Vec::new();
        vint(&mut file, 2);
        vint(&mut file, 0x02);
        vint(&mut file, data.len() as u64); // data size (flag 0x02 field, before file body)
        vint(&mut file, 0x04); // file flags: crc present
        vint(&mut file, data.len() as u64); // unpacked size
        vint(&mut file, 0); // attributes
        file.extend_from_slice(&crate::nntp::crc32(data).to_le_bytes());
        vint(&mut file, 0); // compression info: method 0 stored
        vint(&mut file, 1); // host OS unix
        vint(&mut file, name.len() as u64);
        file.extend_from_slice(name.as_bytes());
        header(&mut out, &file);
        out.extend_from_slice(data);
        // end head: type 5, flags 0
        let mut end = Vec::new();
        vint(&mut end, 5);
        vint(&mut end, 0);
        vint(&mut end, 0);
        header(&mut out, &end);
        out
    }

    #[test]
    fn rar5_stored_extract_and_delete() {
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-rar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let arc = dir.join("pack.rar");
        std::fs::write(&arc, rar5_stored("inner.txt", b"rar payload!")).unwrap();
        let n = unpack_dir(&dir, &["pack.rar".to_string()], None).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(dir.join("inner.txt")).unwrap(), b"rar payload!");
        assert!(!arc.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encrypted_zip_password_right_and_wrong() {
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-enc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let make = |path: &Path| {
            let f = std::fs::File::create(path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .with_aes_encryption(zip::AesMode::Aes256, "s3cret");
            w.start_file("secret.txt", opts).unwrap();
            std::io::Write::write_all(&mut w, b"classified").unwrap();
            w.finish().unwrap();
        };
        let arc = dir.join("enc.zip");
        make(&arc);
        // Wrong password: Err, archive kept, nothing extracted.
        let err = unpack_dir(&dir, &["enc.zip".to_string()], Some("wrong")).unwrap_err();
        assert!(err.contains("enc.zip"), "{err}");
        assert!(arc.exists());
        assert!(!dir.join("secret.txt").exists());
        // No password: also fails, kept.
        assert!(unpack_dir(&dir, &["enc.zip".to_string()], None).is_err());
        assert!(arc.exists());
        // Right password: extracts, archive deleted.
        let n = unpack_dir(&dir, &["enc.zip".to_string()], Some("s3cret")).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(dir.join("secret.txt")).unwrap(), b"classified");
        assert!(!arc.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nzb_meta_password_flows_to_encrypted_zip() {
        // NZB meta password -> nzb_password() -> unpack_dir encrypted extract.
        let xml = concat!(
            "<nzb><head><meta type=\"password\">s3cret</meta></head>",
            "<file subject=\"p &quot;enc.zip&quot;\"><segments><segment bytes=\"10\" number=\"1\">a@b</segment></segments></file></nzb>"
        );
        let nzb = crate::nntp::parse_nzb(xml).unwrap();
        let pw = crate::nntp::nzb_password(&nzb, "https://x/r.nzb");
        assert_eq!(pw.as_deref(), Some("s3cret"));
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-nzbpw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let arc = dir.join("enc.zip");
        {
            let f = std::fs::File::create(&arc).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .with_aes_encryption(zip::AesMode::Aes256, "s3cret");
            w.start_file("secret.txt", opts).unwrap();
            std::io::Write::write_all(&mut w, b"from nzb password").unwrap();
            w.finish().unwrap();
        }
        let n = unpack_dir(&dir, &["enc.zip".to_string()], pw.as_deref()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(dir.join("secret.txt")).unwrap(), b"from nzb password");
        assert!(!arc.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encrypted_7z_password() {
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-enc7z-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("s.txt"), b"seven secret").unwrap();
        let arc = dir.join("enc.7z");
        sevenz_rust2::compress_to_path_encrypted(
            &src,
            &arc,
            sevenz_rust2::Password::from("pw7"),
        )
        .unwrap();
        assert!(unpack_dir(&dir, &["enc.7z".to_string()], Some("nope")).is_err());
        assert!(arc.exists());
        let n = unpack_dir(&dir, &["enc.7z".to_string()], Some("pw7")).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(dir.join("s.txt")).unwrap(), b"seven secret");
        assert!(!arc.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_zip_001_extract_and_delete() {
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Build zip, split bytes into .001/.002.
        let zip_path = dir.join("tmp.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("s.txt", opts).unwrap();
            std::io::Write::write_all(&mut w, b"split zip payload bytes here").unwrap();
            w.finish().unwrap();
        }
        let bytes = std::fs::read(&zip_path).unwrap();
        std::fs::remove_file(&zip_path).unwrap();
        let mid = bytes.len() / 2;
        std::fs::write(dir.join("pack.zip.001"), &bytes[..mid]).unwrap();
        std::fs::write(dir.join("pack.zip.002"), &bytes[mid..]).unwrap();
        let n = unpack_dir(&dir, &["pack.zip.001".to_string()], None).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read(dir.join("s.txt")).unwrap(), b"split zip payload bytes here");
        assert!(!dir.join("pack.zip.001").exists());
        assert!(!dir.join("pack.zip.002").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_volume_rar_parts_listed() {
        let dir = std::env::temp_dir().join(format!("webcraft-unpack-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.part1.rar", "a.part2.rar", "a.part3.rar", "b.rar", "b.r00", "b.r01", "c.zip"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let mut p: Vec<String> = archive_parts(&dir, "a.part1.rar")
            .iter()
            .map(|x| x.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        p.sort();
        assert_eq!(p, vec!["a.part1.rar", "a.part2.rar", "a.part3.rar"]);
        let mut q: Vec<String> = archive_parts(&dir, "b.rar")
            .iter()
            .map(|x| x.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        q.sort();
        assert_eq!(q, vec!["b.r00", "b.r01", "b.rar"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
