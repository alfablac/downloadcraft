use url::Url;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Variant { pub uri: String, pub bandwidth: u64, pub resolution: String, pub codecs: String, pub audio_group: String, pub subtitles_group: String }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rendition { pub group_id: String, pub name: String, pub uri: Option<String>, pub kind: String, pub default: bool, pub language: String }
#[derive(Clone, Debug, Default)]
pub struct MediaPlaylist {
    pub segments: Vec<String>,
    pub init_uri: Option<String>,
    pub byte_ranges: Vec<Option<(u64, Option<u64>)>>, // (length, offset)
    pub keys: Vec<Option<String>>,
    pub end_list: bool,
    pub target_duration: f64,
    pub map_byte_range: Option<(u64, Option<u64>)>,
}
#[derive(Clone, Debug, Default)]
pub struct Master { pub variants: Vec<Variant>, pub renditions: Vec<Rendition> }

fn resolve(base: &Url, rel: &str) -> String { base.join(rel.trim()).map(|u| u.to_string()).unwrap_or_else(|_| rel.trim().to_string()) }

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new(); let mut cur = String::new(); let mut in_q = false; let mut parts = Vec::new();
    for c in s.chars() { match c { '"' => { in_q = !in_q; cur.push(c); } ',' if !in_q => { parts.push(std::mem::take(&mut cur)); } _ => cur.push(c) } }
    if !cur.is_empty() { parts.push(cur); }
    for p in parts { if let Some(i) = p.find('=') { out.push((p[..i].trim().to_string(), p[i + 1..].trim().trim_matches('"').to_string())); } }
    out
}
fn attr(attrs: &[(String, String)], k: &str) -> Option<String> { attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()) }

pub fn parse_master(text: &str, base: &Url) -> Master {
    let mut m = Master::default(); let mut pending: Option<Variant> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let a = parse_attrs(rest);
            pending = Some(Variant { bandwidth: attr(&a, "BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0), resolution: attr(&a, "RESOLUTION").unwrap_or_default(), codecs: attr(&a, "CODECS").unwrap_or_default(), audio_group: attr(&a, "AUDIO").unwrap_or_default(), subtitles_group: attr(&a, "SUBTITLES").unwrap_or_default(), uri: String::new() });
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = parse_attrs(rest);
            m.renditions.push(Rendition { group_id: attr(&a, "GROUP-ID").unwrap_or_default(), name: attr(&a, "NAME").unwrap_or_default(), uri: attr(&a, "URI").map(|u| resolve(base, &u)), kind: attr(&a, "TYPE").unwrap_or_default(), default: attr(&a, "DEFAULT").map(|v| v == "YES").unwrap_or(false), language: attr(&a, "LANGUAGE").unwrap_or_default() });
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(mut v) = pending.take() { v.uri = resolve(base, line); m.variants.push(v); }
        }
    }
    m
}

fn parse_range(s: &str) -> (u64, Option<u64>) {
    let mut it = s.split('@');
    let len: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let off = it.next().and_then(|v| v.parse().ok());
    (len, off)
}

pub fn parse_media(text: &str, base: &Url) -> MediaPlaylist {
    let mut p = MediaPlaylist::default();
    let mut cur_range: Option<(u64, Option<u64>)> = None; let mut cur_key: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attrs(rest);
            if let Some(u) = attr(&a, "URI") { p.init_uri = Some(resolve(base, &u)); }
            if let Some(r) = attr(&a, "BYTERANGE") { p.map_byte_range = Some(parse_range(&r)); }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") { cur_range = Some(parse_range(rest)); }
        else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = parse_attrs(rest);
            let method = attr(&a, "METHOD").unwrap_or_default();
            cur_key = if method == "NONE" || method.is_empty() { None } else { Some(method) };
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") { p.target_duration = rest.parse().unwrap_or(0.0); }
        else if line == "#EXT-X-ENDLIST" { p.end_list = true; }
        else if !line.is_empty() && !line.starts_with('#') {
            p.segments.push(resolve(base, line)); p.byte_ranges.push(cur_range.take()); p.keys.push(cur_key.clone());
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
        if let Some(start) = *off { last_end.insert(uri.clone(), start + *len); }
    }
    for i in 0..p.segments.len() {
        let url = p.segments[i].clone();
        match &mut p.byte_ranges[i] {
            Some((len, off)) => {
                *off = off.or_else(|| last_end.get(url.as_str()).copied());
                if let Some(start) = *off { last_end.insert(url, start + *len); }
            }
            None => { last_end.remove(url.as_str()); }
        }
    }
    p
}

pub fn has_key(text: &str) -> bool { text.lines().any(|l| { let l = l.trim(); l.starts_with("#EXT-X-KEY:") && !l.contains("METHOD=NONE") }) }

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Url { Url::parse("https://x.example.com/hls/master.m3u8").unwrap() }
    #[test] fn master_variants() {
        let t = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"EN\",DEFAULT=YES,URI=\"a.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,CODECS=\"avc1,mp4a\",AUDIO=\"aud\"\n720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=400000\n360.m3u8\n";
        let m = parse_master(t, &base());
        assert_eq!(m.variants.len(), 2); assert_eq!(m.variants[0].resolution, "1280x720"); assert_eq!(m.renditions[0].name, "EN");
    }
    #[test] fn media_fmp4() {
        let t = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6,\nseg1.m4s\n#EXT-X-BYTERANGE:100@0\n#EXTINF:6,\nseg2.m4s\n#EXT-X-ENDLIST\n";
        let p = parse_media(t, &Url::parse("https://x.example.com/hls/720.m3u8").unwrap());
        assert_eq!(p.segments.len(), 2); assert!(p.init_uri.is_some()); assert!(p.end_list); assert_eq!(p.byte_ranges[1], Some((100, Some(0))));
    }
    #[test] fn implicit_byterange_offsets_chain_same_url() {
        let t = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n#EXT-X-BYTERANGE:100@200\n#EXTINF:4,\nfile.mp4\n#EXT-X-BYTERANGE:50\n#EXTINF:4,\nfile.mp4\n#EXT-X-BYTERANGE:25\n#EXTINF:4,\nother.mp4\n#EXT-X-ENDLIST\n";
        let p = resolve_implicit_ranges(parse_media(t, &base()));
        assert_eq!(p.map_byte_range, Some((720, Some(0))));
        assert_eq!(p.byte_ranges[0], Some((100, Some(200))));
        assert_eq!(p.byte_ranges[1], Some((50, Some(300))));
        assert_eq!(p.byte_ranges[2], Some((25, None)));
    }
}
