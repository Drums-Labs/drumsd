//! Mount-table summary and contract (ports of `criu_root.parse_mountinfo`, `mount_contract` and `candidate.mount_summary`).

pub type MountRow = [String; 3]; // [mountpoint, fstype, root]

pub const ALLOWED_FSTYPES: [&str; 5] = ["overlay", "proc", "sysfs", "tmpfs", "devpts"];

pub fn parse_mountinfo(text: &str) -> Result<Vec<(String, String, String)>, String> {
    let mut out = vec![];
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (pre, post) = line.split_once(" - ").ok_or_else(|| format!("unparseable mountinfo line: {line:?}"))?;
        let f: Vec<&str> = pre.split_whitespace().collect();
        let g: Vec<&str> = post.split_whitespace().collect();
        if f.len() < 6 || g.len() < 2 {
            return Err(format!("unparseable mountinfo line: {line:?}"));
        }
        out.push((f[4].to_string(), g[0].to_string(), f[3].to_string()));
    }
    Ok(out)
}

/// Sorted [mountpoint, fstype, root] rows, as `mounts-pre-dump.json` stores them.
pub fn summary(text: &str) -> Result<Vec<MountRow>, String> {
    let mut v: Vec<MountRow> = parse_mountinfo(text)?.into_iter().map(|(m, f, r)| [m, f, r]).collect();
    v.sort();
    Ok(v)
}

/// Rows only in `want` and rows only in `got` (empty, empty = equal).
pub fn diff(want: &[MountRow], got: &[MountRow]) -> (Vec<MountRow>, Vec<MountRow>) {
    let mut w = want.to_vec();
    let mut g = got.to_vec();
    w.sort();
    g.sort();
    let missing = w.iter().filter(|r| !g.contains(r)).cloned().collect();
    let extra = g.iter().filter(|r| !w.contains(r)).cloned().collect();
    (missing, extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MI: &str = "636 600 0:52 / / rw,relatime - overlay overlay rw,lowerdir=/mnt/osworld,upperdir=/s/upper,workdir=/s/work\n\
637 636 0:53 / /proc rw,nosuid - proc proc rw\n\
638 636 0:54 / /tmp rw - tmpfs tmpfs rw\n";

    #[test]
    fn summary_and_diff() {
        let s = summary(MI).unwrap();
        assert_eq!(s[0], ["/".to_string(), "overlay".into(), "/".into()]);
        assert_eq!(s.len(), 3);
        let mut other = s.clone();
        other.pop();
        let (missing, extra) = diff(&s, &other);
        assert_eq!(missing.len(), 1);
        assert!(extra.is_empty());
        assert!(parse_mountinfo("garbage").is_err());
    }
}
