//! Fractional index keys for sibling order (`nodes.position`) and, later, the focus list.
//!
//! Keys are strings over a base-62 alphabet in ASCII order, compared bytewise (SQLite's
//! default BINARY collation). There is always room between two keys, so inserting or
//! reordering writes one row. Keys never end in the lowest digit, which keeps that true.

const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn digit_index(c: u8) -> usize {
    DIGITS
        .iter()
        .position(|&d| d == c)
        .expect("position keys only contain base-62 digits")
}

/// The key for the first child of an empty parent.
pub fn first() -> String {
    "V".to_string()
}

/// A key that sorts after `last` (append to the end of a sibling list).
pub fn after(last: &str) -> String {
    let bytes = last.as_bytes();
    // Bump the last digit when possible ("V" -> "W"), otherwise extend ("z" -> "zV").
    if let Some((&tail, head)) = bytes.split_last() {
        let i = digit_index(tail);
        if i + 1 < DIGITS.len() {
            let mut out = head.to_vec();
            out.push(DIGITS[i + 1]);
            return String::from_utf8(out).expect("ascii");
        }
    }
    format!("{last}V")
}

/// A key strictly between `a` and `b` (`None` = open end). Requires `a < b`.
pub fn between(a: Option<&str>, b: Option<&str>) -> String {
    match (a, b) {
        (None, None) => first(),
        (Some(a), None) => after(a),
        (a, Some(b)) => midpoint(a.unwrap_or("").as_bytes(), Some(b.as_bytes())),
    }
}

fn midpoint(a: &[u8], b: Option<&[u8]>) -> String {
    if let Some(b) = b {
        // Copy the shared prefix, treating a missing digit in `a` as the lowest digit.
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(DIGITS[0]) == b[n] {
            n += 1;
        }
        if n > 0 {
            let prefix = String::from_utf8(b[..n].to_vec()).expect("ascii");
            let rest = midpoint(a.get(n..).unwrap_or(&[]), Some(&b[n..]));
            return prefix + &rest;
        }
    }
    let da = a.first().map_or(0, |&c| digit_index(c));
    let db = b
        .and_then(|b| b.first())
        .map_or(DIGITS.len(), |&c| digit_index(c));
    if db - da > 1 {
        return (DIGITS[(da + db) / 2] as char).to_string();
    }
    match b {
        // b is longer than one digit: its first digit alone sorts between a and b.
        Some(b) if b.len() > 1 => (b[0] as char).to_string(),
        _ => {
            let mut out = (DIGITS[da] as char).to_string();
            out.push_str(&midpoint(a.get(1..).unwrap_or(&[]), None));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_sort_in_order() {
        let mut keys = vec![first()];
        for _ in 0..200 {
            let next = after(keys.last().unwrap());
            assert!(
                next > *keys.last().unwrap(),
                "{next} should follow {:?}",
                keys.last()
            );
            keys.push(next);
        }
        assert!(keys.iter().all(|k| k.len() <= 8));
    }

    #[test]
    fn between_is_strictly_between() {
        let cases = [
            ("V", "W"),
            ("V", "k"),
            ("a", "a1"),
            ("zz", "zzV"),
            ("1", "2"),
        ];
        for (a, b) in cases {
            let m = between(Some(a), Some(b));
            assert!(a < m.as_str() && m.as_str() < b, "{a} < {m} < {b}");
        }
        let m = between(None, Some("V"));
        assert!(m.as_str() < "V");
        let m = between(None, Some("1"));
        assert!(m.as_str() < "1" && !m.is_empty());
    }

    #[test]
    fn repeated_inserts_keep_order() {
        let (lo, mut hi) = ("V".to_string(), "W".to_string());
        for _ in 0..100 {
            let m = between(Some(&lo), Some(&hi));
            assert!(lo < m && m < hi);
            hi = m;
        }
    }
}
