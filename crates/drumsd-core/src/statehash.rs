//! The desktop state hash (port of `iarun/checks.py::state_hash` = `osworld_smoke` STATE_SECTIONS, ecstatic 91988fc), so a
//! drumsd hash equals the Python path's hash for the same desktop.

use sha2::{Digest, Sha256};

pub const STATE_SECTIONS: [&str; 3] = ["find ~/Desktop -printf '%P %y %u %g %m %s %l\\n'", "find ~/Desktop -type f -exec sha256sum {} +", "ls -A ~"];

/// Python's str.splitlines() line boundaries.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    let b: Vec<(usize, char)> = s.char_indices().collect();
    let mut i = 0;
    while i < b.len() {
        let (pos, c) = b[i];
        let is_break = matches!(c, '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if is_break {
            out.push(&s[start..pos]);
            let mut next = pos + c.len_utf8();
            if c == '\r' && i + 1 < b.len() && b[i + 1].1 == '\n' {
                next += 1;
                i += 1;
            }
            start = next;
        }
        i += 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// sha256 over the three section outputs, each sorted line-wise; section 2 drops hidden names.
pub fn state_hash(outputs: &[String; 3]) -> String {
    let mut parts = vec![];
    for (i, out) in outputs.iter().enumerate() {
        let mut lines: Vec<&str> = splitlines(out).into_iter().filter(|l| !(i == 2 && l.starts_with('.'))).collect();
        lines.sort();
        parts.push(format!("## {i}\n{}", lines.join("\n")));
    }
    hex::encode(Sha256::digest(parts.join("\n").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n"), vec!["a"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("x\n\ny"), vec!["x", "", "y"]);
    }

    #[test]
    fn hash_matches_the_python_reference() {
        // python3 -c 'import hashlib; parts=["## 0\n a d user user 755 4096 ", "## 1\n", "## 2\nDesktop\nDocuments"];
        //             print(hashlib.sha256("\n".join(parts).encode()).hexdigest())'
        let outs = [" a d user user 755 4096 \n".to_string(), "".to_string(), "Documents\n.cache\nDesktop\n".to_string()];
        let want = {
            let s = "## 0\n a d user user 755 4096 \n## 1\n\n## 2\nDesktop\nDocuments";
            hex::encode(Sha256::digest(s.as_bytes()))
        };
        assert_eq!(state_hash(&outs), want);
    }
}
