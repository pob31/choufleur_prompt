//! Similarity primitives for line matching.
//!
//! `rapidfuzz` 0.5 ships `fuzz::ratio` but not `token_set_ratio`, so the token-set
//! variant is implemented here on top of it. Token-set is the right shape for this
//! problem: ASR drops and duplicates words, and an actor's paraphrase reorders them,
//! but the *set* of content words survives both.

/// Indel-based similarity of two strings, in `[0, 1]`.
///
/// (`rapidfuzz` 0.5 already normalizes to `[0, 1]`, unlike the Python original's
/// 0–100 scale — verified by test, not assumed.)
pub fn ratio(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    rapidfuzz::fuzz::ratio(a.chars(), b.chars())
}

/// Token-set ratio in `[0, 1]`.
///
/// Sorts and dedupes both token sets, then compares three strings built from the
/// intersection `i` and the two differences `d1`, `d2`: `i` vs `i+d1`, `i` vs
/// `i+d2`, and `i+d1` vs `i+d2`. The maximum wins. A large shared core therefore
/// scores high even when one side carries extra material — exactly the situation
/// when a 4-second ASR chunk covers a line and a half.
pub fn token_set_ratio(a: &[&str], b: &[&str]) -> f64 {
    if a.is_empty() || b.is_empty() {
        // No evidence either way; callers treat 0 as "no match".
        return 0.0;
    }
    let mut sa: Vec<&str> = a.to_vec();
    let mut sb: Vec<&str> = b.to_vec();
    sa.sort_unstable();
    sa.dedup();
    sb.sort_unstable();
    sb.dedup();

    let mut inter: Vec<&str> = Vec::new();
    let mut only_a: Vec<&str> = Vec::new();
    let mut only_b: Vec<&str> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < sa.len() && j < sb.len() {
        match sa[i].cmp(sb[j]) {
            std::cmp::Ordering::Equal => {
                inter.push(sa[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                only_a.push(sa[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                only_b.push(sb[j]);
                j += 1;
            }
        }
    }
    only_a.extend_from_slice(&sa[i..]);
    only_b.extend_from_slice(&sb[j..]);

    // With an empty intersection this degenerates to a character-level comparison
    // of the two sorted token sets, which is the standard behaviour and the reason
    // unrelated lines score around 0.3–0.4 rather than 0. That is the noise floor
    // the acceptance threshold has to clear.
    let s_inter = inter.join(" ");
    let s_a = join_with(&inter, &only_a);
    let s_b = join_with(&inter, &only_b);

    let mut best = ratio(&s_inter, &s_a);
    best = best.max(ratio(&s_inter, &s_b));
    best.max(ratio(&s_a, &s_b))
}

/// Dice coefficient over the two token *sets*, in `[0, 1]`: `2|A∩B| / (|A|+|B|)`.
///
/// This is the overlap term [`token_set_ratio`] deliberately throws away. Token-set
/// similarity is near-blind to one side carrying extra material — which is what
/// makes it robust to a chunk spilling across a line boundary, and also what makes
/// it rank a three-line span above the single line a segment actually covered.
/// Multiplying the two gives a score that rewards *hearing the span* and *hearing
/// nothing but the span*, so a span grows only when the extra words were really said.
pub fn token_dice(a: &[&str], b: &[&str]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut sa, mut sb) = (a.to_vec(), b.to_vec());
    sa.sort_unstable();
    sa.dedup();
    sb.sort_unstable();
    sb.dedup();

    let (mut i, mut j, mut shared) = (0usize, 0usize, 0usize);
    while i < sa.len() && j < sb.len() {
        match sa[i].cmp(sb[j]) {
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    2.0 * shared as f64 / (sa.len() + sb.len()) as f64
}

/// Fraction of `needle`'s distinct tokens that appear in `haystack`, in `[0, 1]`.
///
/// Used to demand that every line a multi-line span claims was actually heard.
/// Without it, appending a one-word line ("Oui.") to a span costs almost nothing
/// under any aggregate measure, and the tracker drifts a line ahead of the show
/// on every short interjection in the script.
pub fn token_coverage(needle: &[&str], haystack: &[&str]) -> f64 {
    if needle.is_empty() {
        return 1.0;
    }
    let mut uniq: Vec<&str> = needle.to_vec();
    uniq.sort_unstable();
    uniq.dedup();
    let found = uniq.iter().filter(|t| haystack.contains(t)).count();
    found as f64 / uniq.len() as f64
}

fn join_with(head: &[&str], tail: &[&str]) -> String {
    let mut s = head.join(" ");
    if !tail.is_empty() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&tail.join(" "));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Vec<&str> {
        s.split_whitespace().collect()
    }

    #[test]
    fn identical_token_sets_score_one() {
        assert_eq!(token_set_ratio(&t("alors pars"), &t("alors pars")), 1.0);
    }

    #[test]
    fn order_does_not_matter() {
        let a = token_set_ratio(
            &t("tu ne devrais pas etre ici"),
            &t("ici etre pas ne devrais tu"),
        );
        assert_eq!(a, 1.0);
    }

    #[test]
    fn a_subset_still_scores_high() {
        // A 5-second chunk that only caught half the line.
        let s = token_set_ratio(&t("tu ne devrais pas etre ici"), &t("devrais pas etre ici"));
        assert!(s > 0.95, "subset scored {s}");
    }

    #[test]
    fn extra_material_is_tolerated() {
        // The chunk spilled into the next line.
        let s = token_set_ratio(&t("alors pars"), &t("alors pars je sais bien"));
        assert!(s > 0.9, "superset scored {s}");
    }

    #[test]
    fn unrelated_lines_stay_under_the_noise_floor() {
        // Sharing no words at all still leaves a character-level residue; what
        // matters is that it sits well below `TrackerConfig::accept_threshold`.
        for (a, b) in [
            ("alors pars maintenant", "the king is dead"),
            ("bonjour tout le monde", "she never came back"),
            ("i have seen enough", "va t en immediatement"),
        ] {
            let s = token_set_ratio(&t(a), &t(b));
            assert!(s < 0.5, "{a:?} vs {b:?} scored {s}");
        }
    }

    #[test]
    fn single_substitution_degrades_gracefully() {
        let s = token_set_ratio(
            &t("tu ne devrais pas etre ici"),
            &t("tu ne devrais pas etre la"),
        );
        assert!((0.75..0.95).contains(&s), "paraphrase scored {s}");
    }

    #[test]
    fn empty_input_is_no_evidence() {
        assert_eq!(token_set_ratio(&[], &t("anything")), 0.0);
        assert_eq!(token_set_ratio(&t("anything"), &[]), 0.0);
    }

    #[test]
    fn ratio_bounds() {
        assert_eq!(ratio("", ""), 1.0);
        assert_eq!(ratio("abc", "abc"), 1.0);
        assert!(ratio("abc", "xyz") < 0.1);
    }
}

#[cfg(test)]
mod overlap_tests {
    use super::*;

    fn t(s: &str) -> Vec<&str> {
        s.split_whitespace().collect()
    }

    #[test]
    fn dice_penalizes_material_the_segment_never_covered() {
        let seg = t("alors pars maintenant");
        let one_line = token_dice(&seg, &t("alors pars maintenant"));
        let three_lines = token_dice(
            &seg,
            &t("alors pars maintenant je sais bien mais je reste ici ce soir"),
        );
        assert_eq!(one_line, 1.0);
        assert!(three_lines < 0.6, "padded span scored {three_lines}");
    }

    #[test]
    fn coverage_asks_whether_a_specific_line_was_heard() {
        let seg = t("la lampe s est eteinte vers quatre heures du matin");
        assert_eq!(token_coverage(&t("la lampe s est eteinte"), &seg), 1.0);
        assert_eq!(token_coverage(&t("oui"), &seg), 0.0);
        // An empty line is vacuously covered.
        assert_eq!(token_coverage(&[], &seg), 1.0);
    }
}


/// Similarity on character trigrams, which survives a recogniser that heard the right
/// sounds and cut them in the wrong places.
///
/// The tracker scores a line as `token_set_ratio × token_dice^k`, and the second
/// factor assumes the word boundaries are right. Real theatre transcripts break that
/// assumption constantly without being *wrong* about the sound: "Polyme Store" shares
/// no whole token with "Polymestor", so the dice factor is zero and the product is
/// zero, however well the strings otherwise agree. Measured on Hécube, against the
/// script line each was trying to be (word column = set dice on tokens):
///
/// ```text
/// heard                 script             words  trigrams
/// "Polyme Store"        "Polymestor"        0.00    0.64
/// "en Tassé-le-Bitain"  "entassé le butin"  0.29    0.59
/// "Athéna III-Yenne"    "Athéna Troyenne"   0.40    0.65
/// "le fils des cubes"   "le fils d'Hécube"  0.50    0.62
/// ```
///
/// Every one of those is a line the operator could read correctly off the screen while
/// the matcher scored it near zero. `accept_threshold` is 0.62, so the difference is
/// not academic: on characters these match, on words they do not.
///
/// Trigrams and not a phonetic algorithm because the corpus is French, Dutch and
/// English, and a soundex is a per-language commitment; trigrams are none.
pub fn char_trigram_dice(a: &str, b: &str) -> f64 {
    fn grams(t: &str) -> std::collections::HashSet<[char; 3]> {
        let mut v: Vec<char> = Vec::with_capacity(t.chars().count() + 2);
        v.push(' ');
        v.extend(t.chars());
        v.push(' ');
        v.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (ga, gb) = (grams(a), grams(b));
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    2.0 * ga.intersection(&gb).count() as f64 / (ga.len() + gb.len()) as f64
}

#[cfg(test)]
mod trigram_tests {
    use super::*;

    #[test]
    fn character_similarity_survives_wrong_word_boundaries() {
        // Worth being precise about which half fails, because it is not the obvious
        // one: `token_set_ratio` handles this well (~0.91, it compares joined
        // strings), and the `token_dice` factor the tracker multiplies it by is what
        // collapses to zero — the two share no whole token. Their product is what
        // scores the line, so the line scores nothing.
        let a = ["polyme", "store"];
        let b = ["polymestor"];
        assert!(token_set_ratio(&a, &b) > 0.85);
        assert_eq!(token_dice(&a, &b), 0.0);
        assert_eq!(token_set_ratio(&a, &b) * token_dice(&a, &b), 0.0);
        assert!(char_trigram_dice("polyme store", "polymestor") > 0.55);
    }

    #[test]
    fn it_still_separates_unrelated_lines() {
        // The point is not to make everything match everything.
        assert!(char_trigram_dice("bonjour madame", "le train part demain") < 0.3);
    }

    #[test]
    fn identical_text_scores_one() {
        assert!((char_trigram_dice("hecube", "hecube") - 1.0).abs() < 1e-9);
    }
}
