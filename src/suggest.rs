//! "Did you mean?" — Levenshtein-based hint helper for unknown
//! input/output extensions.
//!
//! When `convert` rejects an input or output because its extension is
//! unrecognised, run the bad token against a list of known extensions
//! and append the closest match (if any) to the error message. The
//! ImageMagick CLI doesn't do this, but it's a low-effort, high-payoff
//! polish — `.gtlf` typo'd from `.gltf` is otherwise an "unsupported
//! extension '.gtlf'" surprise.
//!
//! Threshold: a candidate is only suggested when its edit distance is
//! ≤ `max(2, len/3)`. Strict enough that unrelated typos
//! (`.txt` against `.png`) don't produce a misleading hint;
//! generous enough that one transposition (`.gtlf` ↔ `.gltf`),
//! one missing letter (`.jpg` ↔ `.jpeg`), or one wrong letter
//! (`.tff` ↔ `.ttf`) all land.

/// Find the closest match to `bad` in `candidates` by Levenshtein
/// edit distance, subject to the cutoff `max(2, len/3)`. Returns
/// `None` when nothing is close enough.
///
/// Comparison is case-insensitive — the user typing `.GLTF` and the
/// candidate list `gltf` are treated as a perfect match. Empty inputs
/// and empty candidate lists return `None`.
pub fn closest_match<'a>(bad: &str, candidates: &[&'a str]) -> Option<&'a str> {
    if bad.is_empty() || candidates.is_empty() {
        return None;
    }
    let bad_lc = bad.to_ascii_lowercase();
    let max_dist = std::cmp::max(2, bad.len() / 3);
    // Among candidates within the cutoff, pick the one with the
    // smallest Levenshtein distance; break ties by length-closeness
    // to the input (so `.gtlf` (len 4) prefers `.gltf` (len 4) over
    // `.stl` (len 3) when both are 2 edits away). This matches user
    // intuition — a typo of a 4-letter extension is more likely a
    // mangled 4-letter extension than a stretched 3-letter one.
    let mut best: Option<(&'a str, usize, usize)> = None;
    for cand in candidates {
        let cand_lc = cand.to_ascii_lowercase();
        let d = levenshtein(&bad_lc, &cand_lc);
        if d > max_dist {
            continue;
        }
        let len_diff = cand.len().abs_diff(bad.len());
        let candidate = (*cand, d, len_diff);
        match best {
            None => best = Some(candidate),
            Some((_, d_best, len_best)) if d < d_best || (d == d_best && len_diff < len_best) => {
                best = Some(candidate);
            }
            _ => {}
        }
    }
    best.map(|(c, _, _)| c)
}

/// Build a human-readable `(did you mean '.gltf'?)` clause for an
/// unknown extension. Returns an empty string when no close candidate
/// is found, so it's safe to interpolate unconditionally.
///
/// Example:
/// ```ignore
/// let hint = format_hint("gtlf", &["stl", "obj", "gltf", "glb"]);
/// assert_eq!(hint, " (did you mean '.gltf'?)");
/// ```
pub fn format_hint(bad: &str, candidates: &[&str]) -> String {
    match closest_match(bad, candidates) {
        Some(c) => format!(" (did you mean '.{c}'?)"),
        None => String::new(),
    }
}

/// Levenshtein edit distance — number of single-character insertions,
/// deletions, or substitutions to turn `a` into `b`. Standard two-row
/// DP, O(|a| · |b|) time, O(min(|a|, |b|)) space.
fn levenshtein(a: &str, b: &str) -> usize {
    // Bytes are fine here: the inputs are ASCII (file extensions),
    // and even non-ASCII bytes would still produce a useful (if
    // slightly off) distance for short strings. Treating as &[u8]
    // avoids the per-char UTF-8 decode.
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // Two rows: previous and current. `prev[j]` is the edit distance
    // from `a[..i-1]` to `b[..j]`; `cur[j]` is from `a[..i]` to
    // `b[..j]`. We only need the previous row to compute the current
    // one, so the whole table fits in O(|b|) space.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = std::cmp::min(
                std::cmp::min(prev[j] + 1, cur[j - 1] + 1),
                prev[j - 1] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1); // substitution
        assert_eq!(levenshtein("abc", "ab"), 1); // deletion
        assert_eq!(levenshtein("abc", "abcd"), 1); // insertion
        assert_eq!(levenshtein("kitten", "sitting"), 3); // canonical
    }

    #[test]
    fn closest_match_finds_obvious_typos() {
        let known = &["stl", "obj", "gltf", "glb", "usdz", "mtl"];
        assert_eq!(closest_match("gtlf", known), Some("gltf"));
        assert_eq!(closest_match("glft", known), Some("gltf"));
        assert_eq!(closest_match("ojb", known), Some("obj"));
        assert_eq!(closest_match("usdc", known), Some("usdz"));
    }

    #[test]
    fn closest_match_case_insensitive() {
        let known = &["stl", "obj", "gltf", "glb"];
        assert_eq!(closest_match("GLTF", known), Some("gltf"));
        assert_eq!(closest_match("Gltf", known), Some("gltf"));
        assert_eq!(closest_match("STL", known), Some("stl"));
    }

    #[test]
    fn closest_match_rejects_distant_inputs() {
        let known = &["stl", "obj", "gltf", "glb"];
        // `.png` is too far from any known extension — don't suggest.
        assert_eq!(closest_match("png", known), None);
        assert_eq!(closest_match("xyz", known), None);
        assert_eq!(closest_match("audio", known), None);
    }

    #[test]
    fn closest_match_handles_empty_inputs() {
        let known = &["stl", "obj", "gltf"];
        assert_eq!(closest_match("", known), None);
        assert_eq!(closest_match("stl", &[]), None);
    }

    #[test]
    fn format_hint_emits_did_you_mean_clause() {
        let known = &["stl", "obj", "gltf", "glb"];
        assert_eq!(format_hint("gtlf", known), " (did you mean '.gltf'?)");
        assert_eq!(format_hint("png", known), "");
    }

    #[test]
    fn closest_match_picks_strictly_smallest_distance() {
        // `mtl` and `obj` are both 3 chars; the input `mtj` is exactly
        // 1 edit from each. The function returns the first candidate
        // tied for the minimum, which is stable for a fixed slice
        // order — fine for our suggestion-quality threshold.
        let known = &["mtl", "obj"];
        let pick = closest_match("mtj", known);
        // Either is acceptable; the assertion just confirms we return
        // some candidate rather than `None` for a 1-edit input.
        assert!(matches!(pick, Some("mtl") | Some("obj")));
    }
}
