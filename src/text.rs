//! Search text normalization and the fuzzy-matching helpers search ranks with.
//!
//! Titles, aliases and notes are indexed in a normalized form: lowercase, accents
//! folded (`é` -> `e`), apostrophes removed and other punctuation turned into spaces.
//! Titles and aliases also get a squashed copy with no spaces, so `home-lab`,
//! `home lab` and `homelab` all find each other.

/// Lowercases, folds accents and replaces punctuation with single spaces.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = true; // suppress leading and repeated spaces
    for c in text.chars().flat_map(char::to_lowercase) {
        if matches!(c, '\'' | '\u{2019}' | '\u{2018}' | '`') || is_combining(c) {
            continue;
        }
        let folded = fold(c);
        for f in folded.chars() {
            if f.is_alphanumeric() {
                out.push(f);
                space = false;
            } else if !space {
                out.push(' ');
                space = true;
            }
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The normalized text with every space removed: `home lab` -> `homelab`.
pub fn squash(normalized: &str) -> String {
    normalized.chars().filter(|c| *c != ' ').collect()
}

fn is_combining(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036f}')
}

/// Folds common Latin accented letters to their base letters.
fn fold(c: char) -> String {
    const TABLE: &[(&str, &str)] = &[
        ("àáâãäåāăą", "a"),
        ("çćĉċč", "c"),
        ("ďđ", "d"),
        ("èéêëēĕėęě", "e"),
        ("ĝğġģ", "g"),
        ("ĥħ", "h"),
        ("ìíîïĩīĭįı", "i"),
        ("ĵ", "j"),
        ("ķ", "k"),
        ("ĺļľŀł", "l"),
        ("ñńņňŉ", "n"),
        ("òóôõöøōŏő", "o"),
        ("ŕŗř", "r"),
        ("śŝşšș", "s"),
        ("ţťŧț", "t"),
        ("ùúûüũūŭůűų", "u"),
        ("ŵ", "w"),
        ("ýÿŷ", "y"),
        ("źżž", "z"),
        ("ß", "ss"),
        ("æ", "ae"),
        ("œ", "oe"),
        ("þ", "th"),
        ("ð", "d"),
    ];
    if c.is_ascii() {
        return c.to_string();
    }
    TABLE
        .iter()
        .find(|(from, _)| from.contains(c))
        .map_or_else(|| c.to_string(), |(_, to)| (*to).to_string())
}

/// The text stored in one index column: the normalized form, plus the squashed form
/// on its own line when `squashed` is set and it differs.
pub fn index_form(raw: &str, squashed: bool) -> String {
    let norm = normalize(raw);
    let sq = squash(&norm);
    if squashed && sq != norm {
        format!("{norm}\n{sq}")
    } else {
        norm
    }
}

/// Words that carry no meaning in a to-do search ("tasks for my server").
const NOISE: &[&str] = &[
    "a", "an", "and", "are", "at", "for", "from", "i", "in", "is", "it", "me", "my", "of", "on",
    "or", "our", "the", "this", "that", "to", "with", "task", "tasks", "todo", "todos", "item",
    "items", "stuff", "thing", "things",
];

/// Splits a query into normalized keywords, matched as alternatives. Each raw word is
/// squashed (`home-lab` -> `homelab`). Noise words are dropped unless nothing else is left.
pub fn keywords(query: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for raw in query.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let kw = squash(&normalize(raw));
        if !kw.is_empty() && !all.contains(&kw) {
            all.push(kw);
        }
    }
    let meaningful: Vec<String> = all
        .iter()
        .filter(|k| !NOISE.contains(&k.as_str()))
        .cloned()
        .collect();
    if meaningful.is_empty() {
        all
    } else {
        meaningful
    }
}

/// Overlapping three-character fragments of a word.
pub fn trigrams(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    if chars.len() < 3 {
        return Vec::new();
    }
    let mut out: Vec<String> = chars.windows(3).map(|w| w.iter().collect()).collect();
    out.dedup();
    out
}

/// Optimal string alignment distance (Levenshtein plus adjacent transpositions).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

/// How strongly a keyword matches one piece of normalized text (a title, one alias or
/// the notes), from 0 (no match) to 1 (a whole word). `fuzzy` allows typo matches.
pub fn match_strength(kw: &str, text: &str, fuzzy: bool) -> f64 {
    let words: Vec<&str> = text.split([' ', '\n']).filter(|w| !w.is_empty()).collect();
    let kw_len = kw.chars().count();
    // Whole word, or a run of words written together (`homelab` = `home lab`).
    if words.contains(&kw) || joins_words(kw, &words) {
        return 1.0;
    }
    // One and two-letter keywords only match whole words; anything else is noise.
    if kw_len < 3 {
        return 0.0;
    }
    if words.iter().any(|w| w.starts_with(kw)) {
        return 0.85;
    }
    if words.iter().any(|w| w.contains(kw)) {
        return 0.7;
    }
    if !fuzzy || kw_len < 4 {
        return 0.0;
    }
    let allowed = if kw_len >= 8 { 2 } else { 1 };
    let pairs = words.windows(2).map(|p| format!("{}{}", p[0], p[1]));
    let close = words
        .iter()
        .map(|w| (*w).to_string())
        .chain(pairs)
        .any(|w| w.chars().count().abs_diff(kw_len) <= allowed && edit_distance(kw, &w) <= allowed);
    if close {
        return 0.6;
    }
    // Shared three-letter fragments with one word, for misspellings like skyforj/skyforge.
    let grams = trigrams(kw);
    let best = words
        .iter()
        .map(|w| {
            let wg = trigrams(w);
            grams.iter().filter(|g| wg.contains(g)).count() as f64 / grams.len() as f64
        })
        .fold(0.0, f64::max);
    if best >= 0.6 { 0.5 * best } else { 0.0 }
}

/// True if `kw` is two or more consecutive words written without spaces.
fn joins_words(kw: &str, words: &[&str]) -> bool {
    for start in 0..words.len() {
        let mut joined = String::new();
        for (n, w) in words[start..].iter().enumerate() {
            joined.push_str(w);
            if joined.len() > kw.len() || !kw.starts_with(joined.as_str()) {
                break;
            }
            if n > 0 && joined == kw {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_accents_and_punctuation() {
        assert_eq!(normalize("Café  Déjà-vu!"), "cafe deja vu");
        assert_eq!(
            normalize("Brandon's NAS (home-lab)"),
            "brandons nas home lab"
        );
        assert_eq!(normalize("Straße, Øl"), "strasse ol");
        assert_eq!(normalize("e\u{0301}cole"), "ecole");
        assert_eq!(
            index_form("Home-Lab server", true),
            "home lab server\nhomelabserver"
        );
        assert_eq!(index_form("server", true), "server");
    }

    #[test]
    fn keywords_squash_and_drop_noise() {
        assert_eq!(
            keywords("priority tasks for my server"),
            ["priority", "server"]
        );
        assert_eq!(keywords("home-lab, NAS nas"), ["homelab", "nas"]);
        assert_eq!(keywords("for my"), ["for", "my"]);
        assert!(keywords("  !! ").is_empty());
    }

    #[test]
    fn match_strength_handles_spacing_and_typos() {
        assert_eq!(match_strength("homelab", "home lab server", true), 1.0);
        assert_eq!(match_strength("home", "homelab", true), 0.85);
        assert_eq!(match_strength("lab", "homelab", true), 0.7);
        assert_eq!(match_strength("sevrer", "server migration", true), 0.6);
        assert_eq!(match_strength("servers", "server", true), 0.6);
        assert!(match_strength("skyforj", "skyforge", true) > 0.0);
        assert_eq!(match_strength("sevrer", "server", false), 0.0);
        assert_eq!(match_strength("pc", "gaming pc build", true), 1.0);
        assert_eq!(match_strength("pc", "upcoming", true), 0.0);
        assert_eq!(match_strength("xyzzy", "server", true), 0.0);
        assert_eq!(edit_distance("server", "sevrer"), 1);
    }
}
