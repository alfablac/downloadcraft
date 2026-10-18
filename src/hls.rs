use url::Url;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub resolution: String,
    pub codecs: String,
    pub audio_group: String,
    pub subtitles_group: String,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rendition {
    pub group_id: String,
    pub name: String,
    pub uri: Option<String>,
    pub kind: String,
    pub default: bool,
    pub language: String,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyInfo {
    pub method: String,
    pub uri: Option<String>,
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug, Default)]
pub struct MediaPlaylist {
    pub segments: Vec<String>,
    pub init_uri: Option<String>,
    pub byte_ranges: Vec<Option<(u64, Option<u64>)>>, // (length, offset)
    pub keys: Vec<Option<String>>,
    pub key_infos: Vec<Option<KeyInfo>>,
    pub media_sequence: u64,
    pub end_list: bool,
    pub target_duration: f64,
    pub map_byte_range: Option<(u64, Option<u64>)>,
}
#[derive(Clone, Debug, Default)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,
}

fn resolve(base: &Url, rel: &str) -> String {
    base.join(rel.trim())
        .map(|u| u.to_string())
        .unwrap_or_else(|_| rel.trim().to_string())
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut parts = Vec::new();
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                cur.push(c);
            }
            ',' if !in_q => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    for p in parts {
        if let Some(i) = p.find('=') {
            out.push((
                p[..i].trim().to_string(),
                p[i + 1..].trim().trim_matches('"').to_string(),
            ));
        }
    }
    out
}
fn attr(attrs: &[(String, String)], k: &str) -> Option<String> {
    attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone())
}

pub fn parse_master(text: &str, base: &Url) -> Master {
    let mut m = Master::default();
    let mut pending: Option<Variant> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let a = parse_attrs(rest);
            pending = Some(Variant {
                bandwidth: attr(&a, "BANDWIDTH")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                resolution: attr(&a, "RESOLUTION").unwrap_or_default(),
                codecs: attr(&a, "CODECS").unwrap_or_default(),
                audio_group: attr(&a, "AUDIO").unwrap_or_default(),
                subtitles_group: attr(&a, "SUBTITLES").unwrap_or_default(),
                uri: String::new(),
            });
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = parse_attrs(rest);
            m.renditions.push(Rendition {
                group_id: attr(&a, "GROUP-ID").unwrap_or_default(),
                name: attr(&a, "NAME").unwrap_or_default(),
                uri: attr(&a, "URI").map(|u| resolve(base, &u)),
                kind: attr(&a, "TYPE").unwrap_or_default(),
                default: attr(&a, "DEFAULT").map(|v| v == "YES").unwrap_or(false),
                language: attr(&a, "LANGUAGE").unwrap_or_default(),
            });
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(mut v) = pending.take() {
                v.uri = resolve(base, line);
                m.variants.push(v);
            }
        }
    }
    m
}

/// Parse IV hex (optional 0x prefix). Short values left-padded (right-aligned) to 16 bytes.
pub fn parse_iv(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if hex.is_empty() || hex.len() > 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let padded = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for i in 0..16 {
        iv[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

/// First EXT-X-KEY METHOD value in playlist text, if any.
pub fn key_method(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("#EXT-X-KEY:") {
            return attr(&parse_attrs(rest), "METHOD");
        }
    }
    None
}

/// IV derived from media sequence number: 128-bit big-endian.
pub fn iv_for_sequence(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Decrypt one AES-128-CBC segment, stripping PKCS7 padding.
pub fn decrypt_segment(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, String> {
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(format!(
            "ciphertext length {} not multiple of 16",
            data.len()
        ));
    }
    let mut buf = data.to_vec();
    let pt = Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| format!("AES-128 decrypt failed: {e}"))?;
    Ok(pt.to_vec())
}

fn parse_range(s: &str) -> (u64, Option<u64>) {
    let mut it = s.split('@');
    let len: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let off = it.next().and_then(|v| v.parse().ok());
    (len, off)
}

pub fn parse_media(text: &str, base: &Url) -> MediaPlaylist {
    let mut p = MediaPlaylist::default();
    let mut cur_range: Option<(u64, Option<u64>)> = None;
    let mut cur_key: Option<String> = None;
    let mut cur_key_info: Option<KeyInfo> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            p.media_sequence = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attrs(rest);
            if let Some(u) = attr(&a, "URI") {
                p.init_uri = Some(resolve(base, &u));
            }
            if let Some(r) = attr(&a, "BYTERANGE") {
                p.map_byte_range = Some(parse_range(&r));
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            cur_range = Some(parse_range(rest));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = parse_attrs(rest);
            let method = attr(&a, "METHOD").unwrap_or_default();
            cur_key = if method == "NONE" || method.is_empty() {
                None
            } else {
                Some(method.clone())
            };
            cur_key_info = if method == "NONE" || method.is_empty() {
                None
            } else {
                Some(KeyInfo {
                    method,
                    uri: attr(&a, "URI").map(|u| resolve(base, &u)),
                    iv: attr(&a, "IV").and_then(|v| parse_iv(&v)),
                })
            };
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            p.target_duration = rest.parse().unwrap_or(0.0);
        } else if line == "#EXT-X-ENDLIST" {
            p.end_list = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            p.segments.push(resolve(base, line));
            p.byte_ranges.push(cur_range.take());
            p.keys.push(cur_key.clone());
            p.key_infos.push(cur_key_info.clone());
        }
    }
    p
}

/// Fill implicit BYTERANGE offsets: omitted offset = end of previous range on same URL
/// (HLS spec 4.3.2.2). Chains EXT-X-MAP range too. Without this, engine defaulted
/// missing offset to 0 and re-fetched file head per segment.
pub fn resolve_implicit_ranges(mut p: MediaPlaylist) -> MediaPlaylist {
    use std::collections::HashMap;
    let mut last_end: HashMap<String, u64> = HashMap::new();
    if let (Some(uri), Some((len, off))) = (&p.init_uri, &mut p.map_byte_range) {
        *off = off.or_else(|| last_end.get(uri.as_str()).copied());
        if let Some(start) = *off {
            last_end.insert(uri.clone(), start + *len);
        }
    }
    for i in 0..p.segments.len() {
        let url = p.segments[i].clone();
        match &mut p.byte_ranges[i] {
            Some((len, off)) => {
                *off = off.or_else(|| last_end.get(url.as_str()).copied());
                if let Some(start) = *off {
                    last_end.insert(url, start + *len);
                }
            }
            None => {
                last_end.remove(url.as_str());
            }
        }
    }
    p
}

pub fn has_key(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim();
        l.starts_with("#EXT-X-KEY:") && !l.contains("METHOD=NONE")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Url {
        Url::parse("https://x.example.com/hls/master.m3u8").unwrap()
    }
    #[test]
    fn master_variants() {
        let t = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"EN\",DEFAULT=YES,URI=\"a.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,CODECS=\"avc1,mp4a\",AUDIO=\"aud\"\n720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=400000\n360.m3u8\n";
        let m = parse_master(t, &base());
        assert_eq!(m.variants.len(), 2);
        assert_eq!(m.variants[0].resolution, "1280x720");
        assert_eq!(m.renditions[0].name, "EN");
    }
    #[test]
    fn media_fmp4() {
        let t = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6,\nseg1.m4s\n#EXT-X-BYTERANGE:100@0\n#EXTINF:6,\nseg2.m4s\n#EXT-X-ENDLIST\n";
        let p = parse_media(
            t,
            &Url::parse("https://x.example.com/hls/720.m3u8").unwrap(),
        );
        assert_eq!(p.segments.len(), 2);
        assert!(p.init_uri.is_some());
        assert!(p.end_list);
        assert_eq!(p.byte_ranges[1], Some((100, Some(0))));
    }
    #[test]
    fn parse_key_infos_uri_iv_and_sequence() {
        let t = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:42\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x0000000000000000000000000000002a\n#EXTINF:6,\nseg1.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"key2.bin\"\n#EXTINF:6,\nseg2.ts\n#EXT-X-ENDLIST\n";
        let p = parse_media(t, &base());
        assert_eq!(p.media_sequence, 42);
        assert_eq!(p.keys[0].as_deref(), Some("AES-128"));
        let k0 = p.key_infos[0].as_ref().unwrap();
        assert_eq!(k0.method, "AES-128");
        assert_eq!(k0.uri.as_deref(), Some("https://x.example.com/hls/key.bin"));
        assert_eq!(k0.iv, Some(iv_for_sequence(42)));
        let k1 = p.key_infos[1].as_ref().unwrap();
        assert_eq!(
            k1.uri.as_deref(),
            Some("https://x.example.com/hls/key2.bin")
        );
        assert_eq!(k1.iv, None);
        // default media_sequence = 0
        assert_eq!(
            parse_media("#EXTM3U\n#EXTINF:1,\na.ts\n", &base()).media_sequence,
            0
        );
    }
    #[test]
    fn parse_iv_forms() {
        assert_eq!(parse_iv("0x1"), Some(iv_for_sequence(1)));
        assert_eq!(
            parse_iv("000000000000000000000000000000ff").unwrap()[15],
            0xff
        );
        assert_eq!(parse_iv("0xzz"), None);
        assert_eq!(parse_iv(""), None);
        assert_eq!(parse_iv(&"00".repeat(17)), None);
    }
    #[test]
    fn iv_for_sequence_big_endian() {
        let iv = iv_for_sequence(0x0102_0304_0506_0708);
        assert_eq!(&iv[..8], &[0u8; 8]);
        assert_eq!(&iv[8..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
    #[test]
    fn decrypt_segment_roundtrip_and_errors() {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Enc = cbc::Encryptor<aes::Aes128>;
        let key = [0x2bu8; 16];
        let iv = iv_for_sequence(7);
        let plain = b"hello HLS AES-128 segment!";
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let ct = Enc::new(&key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec();
        assert_eq!(decrypt_segment(&key, &iv, &ct).unwrap(), plain.to_vec());
        // wrong key / tampered padding must error, not panic
        assert!(decrypt_segment(&[0u8; 16], &iv, &ct).is_err());
        let mut bad = ct.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(decrypt_segment(&key, &iv, &bad).is_err());
        assert!(decrypt_segment(&key, &iv, &[1, 2, 3]).is_err());
        assert!(decrypt_segment(&key, &iv, &[]).is_err());
    }
    #[test]
    fn key_method_first() {
        assert_eq!(
            key_method("#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k\"\nseg.ts\n").as_deref(),
            Some("SAMPLE-AES")
        );
        assert_eq!(key_method("#EXTM3U\nseg.ts\n"), None);
    }
    #[test]
    fn implicit_byterange_offsets_chain_same_url() {
        let t = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n#EXT-X-BYTERANGE:100@200\n#EXTINF:4,\nfile.mp4\n#EXT-X-BYTERANGE:50\n#EXTINF:4,\nfile.mp4\n#EXT-X-BYTERANGE:25\n#EXTINF:4,\nother.mp4\n#EXT-X-ENDLIST\n";
        let p = resolve_implicit_ranges(parse_media(t, &base()));
        assert_eq!(p.map_byte_range, Some((720, Some(0))));
        assert_eq!(p.byte_ranges[0], Some((100, Some(200))));
        assert_eq!(p.byte_ranges[1], Some((50, Some(300))));
        assert_eq!(p.byte_ranges[2], Some((25, None)));
    }
}
