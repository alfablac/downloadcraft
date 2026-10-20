//! Sync planner: recursive list both sides, diff by path+size+mtime.
//! Modes: copy (missing only), update (missing + newer/bigger), mirror
//! (update + delete dst extras). Pure planner tested; runner in jobs.
use super::RemoteEntry;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SyncMode {
    Copy,
    Update,
    Mirror,
}
impl SyncMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "mirror" => SyncMode::Mirror,
            "update" => SyncMode::Update,
            _ => SyncMode::Copy,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            SyncMode::Copy => "copy",
            SyncMode::Update => "update",
            SyncMode::Mirror => "mirror",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SyncPlan {
    pub to_copy: Vec<RemoteEntry>, // src entries needing transfer
    pub to_delete: Vec<RemoteEntry>, // dst entries (mirror only)
    pub unchanged: usize,
    pub src_total: u64,
}

/// rel-path -> entry, files only, paths relative to sync root.
fn index(files: &[RemoteEntry], root: &str) -> HashMap<String, RemoteEntry> {
    let root = root.trim_matches('/');
    let mut m = HashMap::new();
    for f in files {
        if f.is_dir {
            continue;
        }
        let rel = if root.is_empty() {
            f.path.trim_matches('/').to_string()
        } else {
            f.path
                .trim_matches('/')
                .strip_prefix(root)
                .unwrap_or(f.path.as_str())
                .trim_matches('/')
                .to_string()
        };
        m.insert(rel, f.clone());
    }
    m
}

pub fn plan(
    mode: SyncMode,
    src_files: &[RemoteEntry],
    src_root: &str,
    dst_files: &[RemoteEntry],
    dst_root: &str,
) -> SyncPlan {
    let s = index(src_files, src_root);
    let d = index(dst_files, dst_root);
    let mut out = SyncPlan {
        src_total: s.values().map(|e| e.size).sum(),
        ..Default::default()
    };
    for (rel, se) in &s {
        match d.get(rel) {
            None => out.to_copy.push(se.clone()),
            Some(de) => {
                let changed = match mode {
                    SyncMode::Copy => false,
                    _ => se.size != de.size || (se.modified > 0 && de.modified > 0 && se.modified > de.modified + 2),
                };
                if changed {
                    out.to_copy.push(se.clone());
                } else {
                    out.unchanged += 1;
                }
            }
        }
    }
    if mode == SyncMode::Mirror {
        for (rel, de) in &d {
            if !s.contains_key(rel) {
                out.to_delete.push(de.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn f(path: &str, size: u64, mt: u64) -> RemoteEntry {
        RemoteEntry {
            id: path.into(),
            name: super::super::base_name(path),
            path: path.into(),
            is_dir: false,
            size,
            modified: mt,
            mime: String::new(),
        }
    }
    #[test]
    fn planner_modes() {
        let src = vec![f("r/a.txt", 10, 100), f("r/b.txt", 20, 100), f("r/c.txt", 5, 100)];
        let dst = vec![f("x/a.txt", 10, 100), f("x/b.txt", 99, 50), f("x/old.txt", 1, 1)];
        let p = plan(SyncMode::Copy, &src, "r", &dst, "x");
        assert_eq!(p.to_copy.len(), 1); // c only
        assert_eq!(p.to_delete.len(), 0);
        let p = plan(SyncMode::Update, &src, "r", &dst, "x");
        assert_eq!(p.to_copy.len(), 2); // b size diff + c
        assert_eq!(p.to_delete.len(), 0);
        let p = plan(SyncMode::Mirror, &src, "r", &dst, "x");
        assert_eq!(p.to_copy.len(), 2);
        assert_eq!(p.to_delete.len(), 1);
        assert_eq!(p.to_delete[0].name, "old.txt");
    }
}
