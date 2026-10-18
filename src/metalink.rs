#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetalinkFile {
    pub name: String,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub urls: Vec<String>,
}

/// Parse Metalink 4 (RFC 5854, `urn:ietf:params:xml:ns:metalink`) and
/// Metalink 3.0 (`http://www.metalinker.org/`). Matching by local tag name
/// only (`tag_name().name()` strips namespace), so both namespaces — and
/// prefix-bound variants — work without namespace bookkeeping.
pub fn parse_metalink(xml: &str) -> Result<Vec<MetalinkFile>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    if doc.root_element().tag_name().name() != "metalink" {
        return Err("not a metalink document".into());
    }
    let mut files = Vec::new();
    for file in doc.descendants().filter(|n| n.has_tag_name("file")) {
        let mut f = MetalinkFile {
            name: file.attribute("name").unwrap_or("").trim().to_string(),
            ..Default::default()
        };
        // Descendants (not just children): v3 nests <hash> under
        // <verification> and <url> under <resources>.
        for n in file.descendants() {
            if n.has_tag_name("size") {
                if f.size.is_none() {
                    f.size = n.text().and_then(|t| t.trim().parse().ok());
                }
            } else if n.has_tag_name("hash") {
                let ty = n
                    .attribute("type")
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .replace(['-', '_'], "");
                if ty == "sha256" && f.sha256.is_none() {
                    f.sha256 = n
                        .text()
                        .map(|t| t.trim().to_ascii_lowercase())
                        .filter(|t| !t.is_empty());
                }
            } else if n.has_tag_name("url") {
                if let Some(t) = n.text().map(|t| t.trim()).filter(|t| !t.is_empty()) {
                    f.urls.push(t.to_string());
                }
            }
        }
        files.push(f);
    }
    if files.is_empty() {
        return Err("metalink has no <file> entries".into());
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta4_sample() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="example.iso">
    <size>12345</size>
    <hash type="sha-256">ABCDEF0123</hash>
    <url priority="1">https://cdn1.example.com/example.iso</url>
    <url priority="2">https://cdn2.example.com/example.iso</url>
  </file>
  <file name="other.bin">
    <url>https://cdn1.example.com/other.bin</url>
  </file>
</metalink>"#;
        let files = parse_metalink(xml).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "example.iso");
        assert_eq!(files[0].size, Some(12345));
        assert_eq!(files[0].sha256.as_deref(), Some("abcdef0123"));
        assert_eq!(
            files[0].urls,
            vec![
                "https://cdn1.example.com/example.iso",
                "https://cdn2.example.com/example.iso"
            ]
        );
        assert_eq!(files[1].size, None);
        assert_eq!(files[1].sha256, None);
    }

    #[test]
    fn metalink3_sample() {
        let xml = r#"<metalink version="3.0" xmlns="http://www.metalinker.org/">
  <files>
    <file name="legacy.zip">
      <size>999</size>
      <verification><hash type="sha256">DEADBEEF</hash></verification>
      <resources>
        <url type="http" preference="100">http://a.example.com/legacy.zip</url>
        <url type="http" preference="90">http://b.example.com/legacy.zip</url>
      </resources>
    </file>
  </files>
</metalink>"#;
        let files = parse_metalink(xml).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "legacy.zip");
        assert_eq!(files[0].size, Some(999));
        assert_eq!(files[0].sha256.as_deref(), Some("deadbeef"));
        assert_eq!(files[0].urls.len(), 2);
        assert!(files[0].urls[1].starts_with("http://b."));
    }

    #[test]
    fn rejects_non_metalink_and_broken_xml() {
        assert!(parse_metalink("<MPD/>").is_err());
        assert!(parse_metalink("<metalink").is_err());
        assert!(
            parse_metalink("<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"/>").is_err(),
            "no files"
        );
        // Non-SHA256 hashes ignored.
        let xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="x"><hash type="md5">abc</hash><url>http://e/x</url></file></metalink>"#;
        assert_eq!(parse_metalink(xml).unwrap()[0].sha256, None);
    }
}
