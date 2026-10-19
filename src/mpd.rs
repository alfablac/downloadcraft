use url::Url;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Representation {
    pub id: String,
    pub bandwidth: u64,
    pub codecs: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: String,
    pub lang: String,
    pub init_url: Option<String>,
    pub segment_urls: Vec<String>,
    pub index_range: Option<String>,
    /// SegmentBase only: `Initialization@range` ("start-end") — init bytes of the single file.
    pub init_range: Option<String>,
    /// SegmentBase only: `SegmentBase@indexRange` ("start-end") — sidx bytes of the single file.
    pub media_range: Option<String>,
    /// True when this rep came from SegmentBase. File already contains init+index+media,
    /// so stream layer must emit one segment with no init — downloading once, not twice.
    pub segment_base: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdaptationSet {
    pub mime_type: String,
    pub content_type: String,
    pub lang: String,
    pub representations: Vec<Representation>,
}

#[derive(Clone, Debug, Default)]
pub struct Mpd {
    pub mpd_type: String,
    pub duration_secs: Option<f64>,
    pub minimum_update_period_secs: Option<f64>,
    pub protected: bool,
    pub sets: Vec<AdaptationSet>,
}

pub fn parse_duration(s: &str) -> Option<f64> {
    // ISO8601 PT duration, e.g. PT1H2M3.5S / PT30S / PT0H5M
    let s = s.strip_prefix("PT")?;
    let (mut total, mut num) = (0.0f64, String::new());
    for c in s.chars() {
        match c {
            'H' => {
                total += num.parse::<f64>().ok()? * 3600.0;
                num.clear();
            }
            'M' => {
                total += num.parse::<f64>().ok()? * 60.0;
                num.clear();
            }
            'S' => {
                total += num.parse::<f64>().ok()?;
                num.clear();
            }
            _ => num.push(c),
        }
    }
    Some(total)
}

fn resolve(base: &Url, rel: &str) -> String {
    match base.join(rel) {
        Ok(mut u) => {
            // Signed sibling files (Reddit packaged-media): join drops auth
            // query. Re-attach minus manifest-select m= when sibling has none.
            if u.query().is_none() && u.path() != base.path() {
                let keep: Vec<(String, String)> = base
                    .query_pairs()
                    .filter(|(k, _)| k != "m")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                if !keep.is_empty() {
                    let mut qp = u.query_pairs_mut();
                    for (k, v) in &keep {
                        qp.append_pair(k, v);
                    }
                }
            }
            u.to_string()
        }
        Err(_) => rel.to_string(),
    }
}

/// Join chained BaseURLs: each BaseURL resolves against previous.
fn chain(base: &Url, nodes: &[String]) -> Url {
    let mut cur = base.clone();
    for n in nodes {
        if let Ok(u) = cur.join(n) {
            cur = u;
        }
    }
    cur
}

fn collect_base_urls(node: roxmltree::Node) -> Vec<String> {
    node.children()
        .filter(|c| c.has_tag_name("BaseURL"))
        .filter_map(|c| c.text().map(|t| t.trim().to_string()))
        .collect()
}

fn expand_template(tpl: &str, rep_id: &str, number: u64, bandwidth: u64, time: u64) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(d) = rest.find('$') {
        out.push_str(&rest[..d]);
        let tail = &rest[d + 1..];
        let Some(end) = tail.find('$') else {
            out.push('$');
            out.push_str(tail);
            return out;
        };
        let token = &tail[..end];
        let (name, fmt) = match token.find('%') {
            Some(i) => (&token[..i], Some(&token[i..])),
            None => (token, None),
        };
        let val = match name {
            "RepresentationID" => rep_id.to_string(),
            "Number" => number.to_string(),
            "Bandwidth" => bandwidth.to_string(),
            "Time" => time.to_string(),
            _ => format!("${token}$"),
        };
        if let Some(f) = fmt {
            // %05d style
            let spec = f.trim_start_matches('%').trim_end_matches('d');
            if let Ok(w) = spec.parse::<usize>() {
                if let Ok(n) = val.parse::<u64>() {
                    out.push_str(&format!("{n:0w$}"));
                    rest = &tail[end + 1..];
                    continue;
                }
            }
            out.push_str(&val);
        } else {
            out.push_str(&val);
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

struct Template {
    media: String,
    initialization: Option<String>,
    start_number: u64,
    duration: Option<f64>,
    timescale: f64,
    timeline: Vec<(u64, i64, i64)>,
} // (t,d,r)

fn parse_template(node: roxmltree::Node) -> Option<Template> {
    if !node.has_tag_name("SegmentTemplate") {
        return None;
    }
    let mut timeline = Vec::new();
    for c in node.children() {
        if c.has_tag_name("SegmentTimeline") {
            for s in c.children() {
                if s.has_tag_name("S") {
                    let t = s.attribute("t").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let d = s.attribute("d").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let r = s.attribute("r").and_then(|v| v.parse().ok()).unwrap_or(0);
                    timeline.push((t, d, r));
                }
            }
        }
    }
    Some(Template {
        media: node.attribute("media")?.to_string(),
        initialization: node.attribute("initialization").map(|s| s.to_string()),
        start_number: node
            .attribute("startNumber")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
        duration: node.attribute("duration").and_then(|v| v.parse().ok()),
        timescale: node
            .attribute("timescale")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0),
        timeline,
    })
}
fn find_template(nodes: &[roxmltree::Node]) -> Option<Template> {
    for n in nodes {
        if let Some(t) = parse_template(*n) {
            return Some(t);
        }
        for c in n.children() {
            if let Some(t) = parse_template(c) {
                return Some(t);
            }
        }
    }
    None
}

fn build_from_template(
    tpl: &Template,
    base: &Url,
    rep: &Representation,
    total_dur: Option<f64>,
) -> (Option<String>, Vec<String>) {
    let init = tpl.initialization.as_ref().map(|i| {
        resolve(
            base,
            &expand_template(i, &rep.id, tpl.start_number, rep.bandwidth, 0),
        )
    });
    let mut urls = Vec::new();
    if !tpl.timeline.is_empty() {
        let mut number = tpl.start_number;
        for &(t, d, r) in &tpl.timeline {
            if d == 0 {
                continue;
            }
            let mut time = t;
            for _ in 0..=(r.max(0)) {
                urls.push(resolve(
                    base,
                    &expand_template(&tpl.media, &rep.id, number, rep.bandwidth, time),
                ));
                number += 1;
                time = time.saturating_add(d as u64);
            }
            if r < 0 {
                // repeat until next S t or total duration
                // ponytail: unbounded -1 handled by caller duration; approximate with next t gap already covered above once, extra repeats computed if total known
                if let Some(td) = total_dur {
                    let total_ticks = (td * tpl.timescale) as u64;
                    let mut time2 = t + d as u64;
                    let mut n2 = number;
                    while time2 < total_ticks {
                        urls.push(resolve(
                            base,
                            &expand_template(&tpl.media, &rep.id, n2, rep.bandwidth, time2),
                        ));
                        time2 += d as u64;
                        n2 += 1;
                    }
                    number = n2;
                }
            }
        }
        return (init, urls);
    }
    if let (Some(dur), Some(td)) = (tpl.duration, total_dur) {
        if dur > 0.0 {
            let seg_dur = dur / tpl.timescale;
            let count = (td / seg_dur).ceil() as u64;
            for i in 0..count {
                let number = tpl.start_number + i;
                let time = (i as f64 * dur) as u64;
                urls.push(resolve(
                    base,
                    &expand_template(&tpl.media, &rep.id, number, rep.bandwidth, time),
                ));
            }
        }
    }
    (init, urls)
}

pub fn parse_mpd(xml: &str, base_url: &Url) -> Result<Mpd, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    if !root.has_tag_name("MPD") {
        return Err("not an MPD".into());
    }
    let protected = xml.contains("ContentProtection");
    let mut mpd = Mpd {
        mpd_type: root.attribute("type").unwrap_or("static").into(),
        duration_secs: root
            .attribute("mediaPresentationDuration")
            .and_then(parse_duration),
        minimum_update_period_secs: root
            .attribute("minimumUpdatePeriod")
            .and_then(parse_duration),
        protected,
        sets: Vec::new(),
    };
    let mpd_bases = collect_base_urls(root);
    for period in root.children().filter(|c| c.has_tag_name("Period")) {
        let period_dur = period
            .attribute("duration")
            .and_then(parse_duration)
            .or(mpd.duration_secs);
        let period_bases = {
            let mut v = mpd_bases.clone();
            v.extend(collect_base_urls(period));
            v
        };
        let period_tpl = find_template(&[period]);
        for aset in period
            .children()
            .filter(|c| c.has_tag_name("AdaptationSet"))
        {
            let aset_bases = {
                let mut v = period_bases.clone();
                v.extend(collect_base_urls(aset));
                v
            };
            let aset_tpl = find_template(&[aset]).or_else(|| period_tpl.clone_ref());
            let mut set = AdaptationSet {
                mime_type: aset.attribute("mimeType").unwrap_or("").into(),
                content_type: aset.attribute("contentType").unwrap_or("").into(),
                lang: aset.attribute("lang").unwrap_or("").into(),
                representations: Vec::new(),
            };
            for rep_node in aset.children().filter(|c| c.has_tag_name("Representation")) {
                let rep_bases = {
                    let mut v = aset_bases.clone();
                    v.extend(collect_base_urls(rep_node));
                    v
                };
                let base = chain(base_url, &rep_bases);
                let mut rep = Representation {
                    id: rep_node.attribute("id").unwrap_or("").into(),
                    bandwidth: rep_node
                        .attribute("bandwidth")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    codecs: rep_node
                        .attribute("codecs")
                        .unwrap_or(aset.attribute("codecs").unwrap_or(""))
                        .into(),
                    width: rep_node.attribute("width").and_then(|v| v.parse().ok()),
                    height: rep_node.attribute("height").and_then(|v| v.parse().ok()),
                    frame_rate: rep_node.attribute("frameRate").unwrap_or("").into(),
                    lang: aset.attribute("lang").unwrap_or("").into(),
                    ..Default::default()
                };
                if set.mime_type.is_empty() {
                    set.mime_type = rep_node.attribute("mimeType").unwrap_or("").into();
                }
                // SegmentTemplate (rep > aset > period)
                let tpl = find_template(&[rep_node])
                    .or_else(|| aset_tpl.clone_ref())
                    .or_else(|| period_tpl.clone_ref());
                let mut handled = false;
                if let Some(t) = tpl {
                    let (init, urls) = build_from_template(&t, &base, &rep, period_dur);
                    rep.init_url = init;
                    rep.segment_urls = urls;
                    handled = true;
                }
                // SegmentList
                for sl in rep_node
                    .children()
                    .chain(aset.children())
                    .filter(|c| c.has_tag_name("SegmentList"))
                {
                    for c in sl.children() {
                        if c.has_tag_name("Initialization") {
                            if let Some(u) = c.attribute("sourceURL") {
                                rep.init_url = Some(resolve(&base, u));
                            }
                        }
                        if c.has_tag_name("SegmentURL") {
                            if let Some(u) = c.attribute("media") {
                                rep.segment_urls.push(resolve(&base, u));
                            }
                        }
                    }
                    handled = true;
                }
                // SegmentBase: single file; Initialization@range=init bytes, SegmentBase@indexRange=sidx.
                for sb in rep_node
                    .children()
                    .chain(aset.children())
                    .filter(|c| c.has_tag_name("SegmentBase"))
                {
                    rep.segment_base = true;
                    if let Some(r) = sb.attribute("indexRange") {
                        rep.index_range = Some(r.into());
                        rep.media_range = Some(r.into());
                    }
                    for c in sb.children() {
                        if c.has_tag_name("Initialization") {
                            if let Some(r) = c.attribute("range") {
                                rep.init_range = Some(r.into());
                                if rep.index_range.is_none() {
                                    rep.index_range = Some(r.into());
                                }
                            }
                        }
                    }
                    // Single URL = whole file. init_url stays None: engine would fetch whole file
                    // again as init (bug). stream layer emits (None, file) — one download.
                    rep.init_url = None;
                    if rep.segment_urls.is_empty() {
                        // No rep BaseURL: file = manifest URL minus manifest-select
                        // params (Reddit ?m=DASHPlaylist.mpd serves manifest at the
                        // variant file URL; keeping query re-fetches XML as media).
                        let mut u = base.clone();
                        let keep: Vec<(String, String)> = u
                            .query_pairs()
                            .filter(|(k, _)| k != "m")
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect();
                        u.set_query(None);
                        if !keep.is_empty() {
                            let mut qp = u.query_pairs_mut();
                            for (k, v) in &keep {
                                qp.append_pair(k, v);
                            }
                        }
                        rep.segment_urls.push(u.to_string());
                    }
                    handled = true;
                }
                let _ = handled;
                set.representations.push(rep);
            }
            if !set.representations.is_empty() {
                mpd.sets.push(set);
            }
        }
    }
    Ok(mpd)
}

trait CloneRef {
    fn clone_ref(&self) -> Option<Template>;
}
impl CloneRef for Option<Template> {
    fn clone_ref(&self) -> Option<Template> {
        self.as_ref().map(|t| Template {
            media: t.media.clone(),
            initialization: t.initialization.clone(),
            start_number: t.start_number,
            duration: t.duration,
            timescale: t.timescale,
            timeline: t.timeline.clone(),
        })
    }
}

pub fn rep_label(r: &Representation, mime: &str) -> String {
    let mut parts = Vec::new();
    if let (Some(w), Some(h)) = (r.width, r.height) {
        parts.push(format!("{w}x{h}"));
    }
    if !r.codecs.is_empty() {
        parts.push(r.codecs.clone());
    }
    if r.bandwidth > 0 {
        parts.push(format!("{} kbps", r.bandwidth / 1000));
    }
    if !r.frame_rate.is_empty() {
        parts.push(format!("{} fps", r.frame_rate));
    }
    if !r.lang.is_empty() {
        parts.push(r.lang.clone());
    }
    if parts.is_empty() {
        format!("{} {}", mime, r.id)
    } else {
        parts.join(" ")
    }
}

pub fn is_protected(xml: &str) -> bool {
    xml.contains("ContentProtection")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Url {
        Url::parse("https://cdn.example.com/vod/manifest.mpd").unwrap()
    }
    #[test]
    fn template_number_pad() {
        let xml = r#"<MPD type="static" mediaPresentationDuration="PT20S"><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate media="seg-$Number%05d$.m4s" initialization="init-$RepresentationID$.mp4" duration="10000" timescale="1000" startNumber="1"/><Representation id="v1" bandwidth="800000" width="1280" height="720" codecs="avc1.64001f"/></AdaptationSet></Period></MPD>"#;
        let m = parse_mpd(xml, &base()).unwrap();
        let r = &m.sets[0].representations[0];
        assert_eq!(
            r.init_url.as_deref(),
            Some("https://cdn.example.com/vod/init-v1.mp4")
        );
        assert_eq!(
            r.segment_urls,
            vec![
                "https://cdn.example.com/vod/seg-00001.m4s",
                "https://cdn.example.com/vod/seg-00002.m4s"
            ]
        );
    }
    #[test]
    fn timeline_urls() {
        let xml = r#"<MPD type="static" mediaPresentationDuration="PT12S"><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate media="s-$Number$-$Time$.m4s" timescale="1000"><SegmentTimeline><S t="0" d="4000" r="2"/></SegmentTimeline></SegmentTemplate><Representation id="v1" bandwidth="100"/></AdaptationSet></Period></MPD>"#;
        let m = parse_mpd(xml, &base()).unwrap();
        assert_eq!(m.sets[0].representations[0].segment_urls.len(), 3);
        assert!(m.sets[0].representations[0].segment_urls[1].contains("-4000"));
    }
    #[test]
    fn segment_list_and_base() {
        let xml = r#"<MPD type="static"><BaseURL>https://a.example.com/x/</BaseURL><Period><AdaptationSet mimeType="audio/mp4"><Representation id="a1" bandwidth="64000"><SegmentList><Initialization sourceURL="init.mp4"/><SegmentURL media="1.m4s"/><SegmentURL media="2.m4s"/></SegmentList></Representation></AdaptationSet><AdaptationSet mimeType="video/mp4"><Representation id="v1" bandwidth="100"><BaseURL>../y/</BaseURL><SegmentBase><Initialization range="0-100"/></SegmentBase></Representation></AdaptationSet></Period></MPD>"#;
        let m = parse_mpd(xml, &base()).unwrap();
        assert_eq!(
            m.sets[0].representations[0].segment_urls,
            vec![
                "https://a.example.com/x/1.m4s",
                "https://a.example.com/x/2.m4s"
            ]
        );
        let sb = &m.sets[1].representations[0];
        assert_eq!(sb.segment_urls, vec!["https://a.example.com/y/"]);
        // SegmentBase: parsed ranges kept, no init URL — file downloaded once (see stream::append_reps).
        assert!(sb.segment_base);
        assert_eq!(sb.init_range.as_deref(), Some("0-100"));
        assert_eq!(sb.init_url, None);
    }
    #[test]
    fn nested_baseurl() {
        let xml = r#"<MPD type="static" mediaPresentationDuration="PT10S"><BaseURL>https://cdn1.example.com/</BaseURL><Period><AdaptationSet mimeType="video/mp4"><BaseURL>vod/</BaseURL><SegmentTemplate media="$RepresentationID$/$Number$.m4s" duration="10" timescale="1"/><Representation id="hi" bandwidth="100"/></AdaptationSet></Period></MPD>"#;
        let m = parse_mpd(xml, &base()).unwrap();
        assert_eq!(
            m.sets[0].representations[0].segment_urls[0],
            "https://cdn1.example.com/vod/hi/1.m4s"
        );
    }
    #[test]
    fn protected_detected() {
        assert!(is_protected("<MPD><ContentProtection/></MPD>"));
    }
    #[test]
    fn multi_period_order() {
        let xml = r#"<MPD type="static" mediaPresentationDuration="PT20S"><Period duration="PT10S"><AdaptationSet mimeType="video/mp4"><SegmentTemplate media="p1-$Number$.m4s" duration="10" timescale="1"/><Representation id="a" bandwidth="1"/></AdaptationSet></Period><Period duration="PT10S"><AdaptationSet mimeType="video/mp4"><SegmentTemplate media="p2-$Number$.m4s" duration="10" timescale="1"/><Representation id="b" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
        let m = parse_mpd(xml, &base()).unwrap();
        assert_eq!(m.sets.len(), 2);
        assert!(m.sets[0].representations[0].segment_urls[0].contains("p1-"));
        assert!(m.sets[1].representations[0].segment_urls[0].contains("p2-"));
    }
    #[test]
    fn duration_parse() {
        assert_eq!(parse_duration("PT1H2M3S"), Some(3723.0));
        assert_eq!(parse_duration("PT30S"), Some(30.0));
    }
}
