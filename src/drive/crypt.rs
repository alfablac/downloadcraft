//! rclone-crypt compatible wrapper backend (backend/crypt/cipher.go):
//! scrypt(16384,8,1,80B) keys [data 32|name 32|tweak 16]; content =
//! "RCLONE\0\0"+24B nonce, 64KiB secretbox blocks, nonce++ LE; names =
//! PKCS7 + AES-EME(tweak) segments, base32hex lower no-pad, dir names too.
//! Wrapper: list decrypts names+sizes, upload encrypts to temp file then
//! inner-upload, download fetches encrypted temp then decrypts.
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::DriveKind;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes256;
use crypto_secretbox::{Key as SbKey, Nonce as SbNonce, XSalsa20Poly1305};
use crypto_secretbox::aead::{Aead, Payload};
use std::sync::Arc;

const MAGIC: &[u8; 8] = b"RCLONE\x00\x00";
const BLOCK_DATA: usize = 64 * 1024;
const OVERHEAD: usize = 16;
const DEFAULT_SALT: [u8; 16] = [
    0xA8, 0x0D, 0xF4, 0x3A, 0x8F, 0xBD, 0x03, 0x08, 0xA7, 0xCA, 0xB8, 0x3E, 0x58, 0x1F, 0x86, 0xB1,
];

pub struct Cipher {
    data_key: [u8; 32],
    name_key: [u8; 32],
    name_tweak: [u8; 16],
}

impl Cipher {
    pub fn new(password: &str, salt: &str) -> Result<Self, String> {
        let salt_bytes: &[u8] = if salt.is_empty() {
            &DEFAULT_SALT
        } else {
            salt.as_bytes()
        };
        // Params len capped 64 by crate; scrypt() output slice (80B) rules.
        let params = scrypt::Params::new(14, 8, 1, 64).map_err(|e| e.to_string())?;
        let mut key = [0u8; 80];
        if password.is_empty() {
            // rclone: empty password = zero keys (tests). Reject in product.
            return Err("crypt password empty".into());
        }
        scrypt::scrypt(password.as_bytes(), salt_bytes, &params, &mut key)
            .map_err(|e| e.to_string())?;
        let mut c = Cipher {
            data_key: [0u8; 32],
            name_key: [0u8; 32],
            name_tweak: [0u8; 16],
        };
        c.data_key.copy_from_slice(&key[0..32]);
        c.name_key.copy_from_slice(&key[32..64]);
        c.name_tweak.copy_from_slice(&key[64..80]);
        Ok(c)
    }

    // ── EME (Halevi-Rogaway) over 16B blocks ─────────────────────
    fn eme(&self, tweak: &[u8; 16], input: &[u8], encrypt: bool) -> Vec<u8> {
        let cipher = Aes256::new_from_slice(&self.name_key).expect("32B key");
        assert!(!input.is_empty() && input.len().is_multiple_of(16));
        eme_transform(&cipher, tweak, input, encrypt)
    }

    fn encrypt_segment(&self, plain: &str) -> String {
        if plain.is_empty() {
            return String::new();
        }
        // PKCS7 pad to 16.
        let b = plain.as_bytes();
        let pad = 16 - (b.len() % 16);
        let mut v = b.to_vec();
        v.extend(std::iter::repeat_n(pad as u8, pad));
        let ct = self.eme(&self.name_tweak, &v, true);
        base32hex_lower(&ct)
    }

    fn decrypt_segment(&self, s: &str) -> Result<String, String> {
        if s.is_empty() {
            return Ok(String::new());
        }
        let raw = base32hex_decode(s)?;
        if raw.is_empty() || raw.len() % 16 != 0 || raw.len() > 2048 {
            return Err("crypt: bad encrypted name length".into());
        }
        let pt = self.eme(&self.name_tweak, &raw, false);
        let pad = *pt.last().ok_or("crypt: empty name")? as usize;
        if pad == 0 || pad > 16 || pad > pt.len() {
            return Err("crypt: bad name padding (wrong password?)".into());
        }
        if pt[pt.len() - pad..].iter().any(|&x| x as usize != pad) {
            return Err("crypt: bad name padding (wrong password?)".into());
        }
        String::from_utf8(pt[..pt.len() - pad].to_vec())
            .map_err(|_| "crypt: name not utf8".to_string())
    }

    pub fn encrypt_path(&self, path: &str) -> String {
        if path.is_empty() {
            return String::new();
        }
        path.split('/')
            .map(|s| self.encrypt_segment(s))
            .collect::<Vec<_>>()
            .join("/")
    }
    pub fn decrypt_path(&self, path: &str) -> Result<String, String> {
        if path.is_empty() {
            return Ok(String::new());
        }
        let mut out = Vec::new();
        for s in path.split('/') {
            out.push(self.decrypt_segment(s)?);
        }
        Ok(out.join("/"))
    }

    // ── file content ─────────────────────────────────────────────
    pub fn encrypted_size(size: u64) -> u64 {
        let blocks = size / BLOCK_DATA as u64;
        let res = size % BLOCK_DATA as u64;
        let mut out = 32 + blocks * (OVERHEAD as u64 + BLOCK_DATA as u64);
        if res != 0 || size == 0 {
            // rclone: residue block only when residue != 0; empty file = header only.
            if res != 0 {
                out += OVERHEAD as u64 + res;
            }
        }
        out
    }
    pub fn decrypted_size(size: u64) -> Result<u64, String> {
        if size < 32 {
            return Err("crypt: encrypted file too short".into());
        }
        let size = size - 32;
        let block = (OVERHEAD + BLOCK_DATA) as u64;
        let blocks = size / block;
        let res = size % block;
        let mut out = blocks * BLOCK_DATA as u64;
        if res != 0 {
            if res <= OVERHEAD as u64 {
                return Err("crypt: bad block header".into());
            }
            out += res - OVERHEAD as u64;
        }
        Ok(out)
    }

    pub fn encrypt_file(
        &self,
        src: &std::path::Path,
        dst: &std::path::Path,
    ) -> Result<(), String> {
        use std::io::{Read, Write};
        let mut inp = std::fs::File::open(src).map_err(|e| e.to_string())?;
        let mut out = std::fs::File::create(dst).map_err(|e| e.to_string())?;
        let nonce_bytes = rand_nonce();
        out.write_all(MAGIC).map_err(|e| e.to_string())?;
        out.write_all(&nonce_bytes).map_err(|e| e.to_string())?;
        let sb = XSalsa20Poly1305::new(SbKey::from_slice(&self.data_key));
        let mut nonce = nonce_bytes;
        let mut buf = vec![0u8; BLOCK_DATA];
        loop {
            let mut filled = 0usize;
            while filled < BLOCK_DATA {
                let n = inp.read(&mut buf[filled..]).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let ct = sb
                .encrypt(
                    SbNonce::from_slice(&nonce),
                    Payload {
                        msg: &buf[..filled],
                        aad: &[],
                    },
                )
                .map_err(|_| "crypt encrypt failed".to_string())?;
            out.write_all(&ct).map_err(|e| e.to_string())?;
            nonce_inc(&mut nonce);
        }
        out.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn decrypt_file(
        &self,
        src: &std::path::Path,
        dst: &std::path::Path,
    ) -> Result<(), String> {
        use std::io::{Read, Write};
        let mut inp = std::fs::File::open(src).map_err(|e| e.to_string())?;
        let mut hdr = [0u8; 32];
        inp.read_exact(&mut hdr).map_err(|_| "crypt: file too short".to_string())?;
        if &hdr[..8] != MAGIC {
            return Err("crypt: bad magic (not rclone-crypt file / wrong password)".into());
        }
        let mut nonce = [0u8; 24];
        nonce.copy_from_slice(&hdr[8..32]);
        let sb = XSalsa20Poly1305::new(SbKey::from_slice(&self.data_key));
        let mut out = std::fs::File::create(dst).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; BLOCK_DATA + OVERHEAD];
        loop {
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = inp.read(&mut buf[filled..]).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            if filled <= OVERHEAD {
                return Err("crypt: truncated block".into());
            }
            let pt = sb
                .decrypt(
                    SbNonce::from_slice(&nonce),
                    Payload {
                        msg: &buf[..filled],
                        aad: &[],
                    },
                )
                .map_err(|_| "crypt: decrypt failed - bad password?".to_string())?;
            out.write_all(&pt).map_err(|e| e.to_string())?;
            nonce_inc(&mut nonce);
        }
        out.flush().map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn nonce_inc(n: &mut [u8; 24]) {
    for b in n.iter_mut() {
        let (v, carry) = b.overflowing_add(1);
        *b = v;
        if !carry {
            break;
        }
    }
}

fn rand_nonce() -> [u8; 24] {
    // OS entropy: /dev/urandom unix; Windows via time+pid hash chain
    // (secretbox nonce reuse with same key is bad but nonce is random
    // 192-bit space; upgrade: use getrandom via crypto_secretbox rand).
    let mut out = [0u8; 24];
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut out).is_ok() {
                return out;
            }
        }
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ ((std::process::id() as u128) << 64);
    for b in &mut out {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = (seed >> 64) as u8;
    }
    out
}

// rfjakob/eme Transform ported line-by-line (tabulateL, multByTwo,
// xor masks, shared encrypt/decrypt structure with AES direction swap).
fn eme_transform(cipher: &Aes256, tweak: &[u8; 16], input: &[u8], encrypt: bool) -> Vec<u8> {
    fn mult_by_two(v: &[u8; 16]) -> [u8; 16] {
        let mut tmp = [0u8; 16];
        tmp[0] = (v[0] << 1) ^ (if v[15] >= 128 { 135 } else { 0 });
        for j in 1..16 {
            tmp[j] = (v[j] << 1).wrapping_add(v[j - 1] >> 7);
        }
        tmp
    }
    fn aes(cipher: &Aes256, blk: &mut [u8; 16], encrypt: bool) {
        let b = aes::Block::from_mut_slice(blk);
        if encrypt {
            cipher.encrypt_block(b);
        } else {
            cipher.decrypt_block(b);
        }
    }
    let m = input.len() / 16;
    // tabulateL: Li starts AESenc(0), each table entry = multByTwo(prev).
    let mut li = [0u8; 16];
    aes(cipher, &mut li, true);
    let mut lt: Vec<[u8; 16]> = Vec::with_capacity(m);
    for _ in 0..m {
        li = mult_by_two(&li);
        lt.push(li);
    }
    let mut c = input.to_vec();
    // First pass: PPPj = AES(Pj xor Lj)
    for j in 0..m {
        let mut blk = [0u8; 16];
        blk.copy_from_slice(&c[j * 16..j * 16 + 16]);
        for k in 0..16 {
            blk[k] ^= lt[j][k];
        }
        aes(cipher, &mut blk, encrypt);
        c[j * 16..j * 16 + 16].copy_from_slice(&blk);
    }
    // MP = xor(PPPj) xor T ; MC = AES(MP)
    let mut mp = [0u8; 16];
    mp.copy_from_slice(&c[0..16]);
    for k in 0..16 {
        mp[k] ^= tweak[k];
    }
    for j in 1..m {
        for k in 0..16 {
            mp[k] ^= c[j * 16 + k];
        }
    }
    let mut mc = mp;
    aes(cipher, &mut mc, encrypt);
    // M = MP xor MC; CCCj (j>=1) = PPPj xor M (M doubles each step)
    let mut mm = [0u8; 16];
    for k in 0..16 {
        mm[k] = mp[k] ^ mc[k];
    }
    for j in 1..m {
        mm = mult_by_two(&mm);
        for k in 0..16 {
            c[j * 16 + k] ^= mm[k];
        }
    }
    // CCC1 = MC xor T xor xor(CCCj j>=1)
    let mut ccc1 = [0u8; 16];
    for k in 0..16 {
        ccc1[k] = mc[k] ^ tweak[k];
    }
    for j in 1..m {
        for k in 0..16 {
            ccc1[k] ^= c[j * 16 + k];
        }
    }
    c[0..16].copy_from_slice(&ccc1);
    // Final pass: Cj = AES(CCCj) xor Lj
    for j in 0..m {
        let mut blk = [0u8; 16];
        blk.copy_from_slice(&c[j * 16..j * 16 + 16]);
        aes(cipher, &mut blk, encrypt);
        for k in 0..16 {
            blk[k] ^= lt[j][k];
        }
        c[j * 16..j * 16 + 16].copy_from_slice(&blk);
    }
    c
}

fn base32hex_lower(data: &[u8]) -> String {
    const T: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
    let mut out = String::new();
    let mut bits = 0u32;
    let mut nbits = 0u32;
    for &b in data {
        bits = (bits << 8) | b as u32;
        nbits += 8;
        while nbits >= 5 {
            nbits -= 5;
            out.push(T[((bits >> nbits) & 31) as usize] as char);
        }
    }
    if nbits > 0 {
        out.push(T[((bits << (5 - nbits)) & 31) as usize] as char);
    }
    out
}

fn base32hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.ends_with('=') {
        return Err("crypt: bad base32 name".into());
    }
    let mut out = Vec::new();
    let mut bits = 0u32;
    let mut nbits = 0u32;
    for c in s.chars() {
        let v = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'a'..='v' => c as u32 - 'a' as u32 + 10,
            'A'..='V' => c as u32 - 'A' as u32 + 10,
            _ => return Err(format!("crypt: bad base32 char {c}")),
        };
        bits = (bits << 5) | v;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Ok(out)
}

pub struct CryptBackend {
    inner: Arc<dyn DriveBackend>,
    cipher: Arc<Cipher>,
}

impl CryptBackend {
    pub fn new(
        inner: Arc<dyn DriveBackend>,
        password: String,
        salt: String,
    ) -> Result<Self, String> {
        Ok(Self {
            inner,
            cipher: Arc::new(Cipher::new(&password, &salt)?),
        })
    }
}

#[async_trait::async_trait]
impl DriveBackend for CryptBackend {
    fn kind(&self) -> DriveKind {
        self.inner.kind()
    }
    async fn about(&self) -> Result<String, String> {
        self.inner.about().await
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let enc = self.cipher.encrypt_path(path);
        let raw = self.inner.list(&enc).await?;
        let mut out = Vec::new();
        for mut e in raw {
            // Skip undecryptable names (foreign files in crypt dir).
            match self.cipher.decrypt_path(&e.name) {
                Ok(name) => {
                    e.name = name.clone();
                    e.path = super::join_path(path, &name);
                    if !e.is_dir {
                        if let Ok(sz) = Cipher::decrypted_size(e.size) {
                            e.size = sz;
                        }
                    }
                    out.push(e);
                }
                Err(_) => continue,
            }
        }
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        let enc = self.cipher.encrypt_path(path);
        self.inner.mkdir(&enc).await
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        let mut e = entry.clone();
        e.path = self.cipher.encrypt_path(&entry.path);
        e.id = if entry.id == entry.path || e.id.is_empty() {
            e.path.clone()
        } else {
            entry.id.clone()
        };
        // id-based backends: id already correct; path-based need enc path.
        // Heuristic: if inner resolves by id (non-path id), keep id.
        self.inner.delete(&e).await
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let mut e = entry.clone();
        e.path = self.cipher.encrypt_path(&entry.path);
        if e.id == entry.path {
            e.id = e.path.clone();
        }
        let enc_new = self.cipher.encrypt_segment(new_name);
        self.inner.rename(&e, &enc_new).await
    }
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        parallel: u32,
        prog: &Progress,
    ) -> Result<(), String> {
        let mut e = entry.clone();
        e.path = self.cipher.encrypt_path(&entry.path);
        if e.id == entry.path {
            e.id = e.path.clone();
        }
        let tmp = dest.with_extension("wc-crypt-tmp");
        let r = self.inner.download(&e, &tmp, parallel, prog).await;
        if let Err(x) = r {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(x);
        }
        let cipher = self.cipher.clone();
        let tmp2 = tmp.clone();
        let dst = dest.to_path_buf();
        tokio::task::spawn_blocking(move || cipher.decrypt_file(&tmp2, &dst))
            .await
            .map_err(|e| e.to_string())??;
        let _ = tokio::fs::remove_file(&tmp).await;
        Ok(())
    }
    async fn upload(
        &self,
        local: &std::path::Path,
        dest_dir: &str,
        name: &str,
        prog: &Progress,
    ) -> Result<RemoteEntry, String> {
        let tmpdir = std::env::temp_dir().join("webcraft-crypt");
        std::fs::create_dir_all(&tmpdir).map_err(|e| e.to_string())?;
        let enc_name = self.cipher.encrypt_segment(name);
        let tmp = tmpdir.join(&enc_name);
        let cipher = self.cipher.clone();
        let src = local.to_path_buf();
        let tmp2 = tmp.clone();
        tokio::task::spawn_blocking(move || cipher.encrypt_file(&src, &tmp2))
            .await
            .map_err(|e| e.to_string())??;
        let enc_dir = self.cipher.encrypt_path(dest_dir);
        let r = self.inner.upload(&tmp, &enc_dir, &enc_name, prog).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        let mut e = r?;
        e.name = name.to_string();
        e.path = super::join_path(dest_dir, name);
        e.size = super::http_util::file_len(local);
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // rclone crypt test vectors (cipher_test.go): password "" -> zero keys
    // only used upstream for tests; here verify self round-trip + sizes +
    // known plaintext name with fixed password vs rclone documented vector.
    #[test]
    fn sizes() {
        assert_eq!(Cipher::encrypted_size(0), 32);
        assert_eq!(Cipher::encrypted_size(1), 32 + 16 + 1);
        assert_eq!(Cipher::encrypted_size(65536), 32 + 16 + 65536);
        assert_eq!(Cipher::encrypted_size(65537), 32 + (16 + 65536) + (16 + 1));
        assert_eq!(Cipher::decrypted_size(32).unwrap(), 0);
        assert_eq!(Cipher::decrypted_size(32 + 16 + 5).unwrap(), 5);
    }
    #[test]
    fn name_roundtrip_and_file_roundtrip() {
        let c = Cipher::new("correct horse battery staple", "").unwrap();
        for name in ["a", "hello.txt", "My Documents", "ünïcödé file.mp4", "12345678901234567890-long-name-for-multiblock-eme-test.bin"] {
            let enc = c.encrypt_segment(name);
            assert_ne!(enc, name);
            assert_eq!(c.decrypt_segment(&enc).unwrap(), name, "eme roundtrip {name}");
        }
        let p = c.encrypt_path("dir/sub/file.txt");
        assert_eq!(c.decrypt_path(&p).unwrap(), "dir/sub/file.txt");
        // File roundtrip incl multi-block.
        let dir = std::env::temp_dir().join(format!("wc-crypt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("plain.bin");
        let enc = dir.join("enc.bin");
        let dec = dir.join("dec.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        c.encrypt_file(&src, &enc).unwrap();
        assert_eq!(
            std::fs::metadata(&enc).unwrap().len(),
            Cipher::encrypted_size(data.len() as u64)
        );
        let raw = std::fs::read(&enc).unwrap();
        assert_eq!(&raw[..8], MAGIC);
        c.decrypt_file(&enc, &dec).unwrap();
        assert_eq!(std::fs::read(&dec).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn wrong_password_fails() {
        let a = Cipher::new("pw-one", "").unwrap();
        let b = Cipher::new("pw-two", "").unwrap();
        let enc = a.encrypt_segment("secret.txt");
        assert!(b.decrypt_segment(&enc).is_err() || b.decrypt_segment(&enc).unwrap() != "secret.txt");
    }
    #[test]
    fn rclone_zero_key_vectors() {
        // rclone cipher_test.go: empty password+salt -> all-zero keys.
        let c = Cipher {
            data_key: [0u8; 32],
            name_key: [0u8; 32],
            name_tweak: [0u8; 16],
        };
        assert_eq!(c.encrypt_segment("1"), "p0e52nreeaj0a5ea7s64m4j72s");
        assert_eq!(c.encrypt_segment("123"), "qgm4avr35m5loi1th53ato71v0");
        assert_eq!(
            c.encrypt_segment("1234567890123456"),
            "mijbj0frqf6ms7frcr6bd9h0env53jv96pjaaoirk7forcgpt70g"
        );
        assert_eq!(c.decrypt_segment("p0e52nreeaj0a5ea7s64m4j72s").unwrap(), "1");
    }
    #[test]
    fn base32_roundtrip() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(base32hex_decode(&base32hex_lower(&data)).unwrap(), data);
        }
    }
}
