//! The interim script model for Phase 0 and its precomputed match index.
//!
//! Deliberately *not* the show file format of notation §11 — that arrives in
//! Phase 1 (M1.1) and retires this module's serde types at M1.4. What survives is
//! [`PreparedScript`]: the index the matcher actually runs against, which the real
//! show file will feed just as well.
//!
//! Language inheritance follows notation §8.2 — `line → character → scene → act →
//! show default`, most specific wins — because getting it wrong means forcing
//! Whisper to the wrong language, which is the single most expensive ASR mistake
//! available.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::lang::{LangCode, MatchText, NormalizerRegistry};

/// Maximum number of script lines one transcript segment may be matched against.
pub const MAX_SPAN: usize = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Character {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<Vec<LangCode>>,
    /// Logical channel indices carrying this character (venue mapping is a
    /// separate concern — see the devplan's venue-vs-show config split).
    #[serde(default)]
    pub channels: Vec<u16>,
    /// The characters this one is made of, when it is a group.
    ///
    /// *Hécube, pas Hécube* gives lines to `LE CHŒUR`, and the chorus is Éric, Gaël,
    /// Élissa and Séphora. On one zone mic that costs nothing; on a patched multitrack
    /// show it is the difference between a chorus line matching all four of their
    /// channels and matching none of them, because a segment arrives stamped with the
    /// performer's name and `LE CHŒUR` is not it.
    ///
    /// **Never inferred.** No rule can know who a chorus is — the operator said so:
    /// *"the converter can't guess this."* It is written down during prep, by the
    /// person who knows, and left empty until then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionMeta {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<Vec<LangCode>>,
}

/// What a line *is*, as distinct from what it says.
///
/// Presentational, and deliberately not a matching rule. A didascalie is usually
/// unspoken, but "usually" is not a thing to build on: in Hécube the cast list
/// contains `SÉPHORA, lit les didascalies` — a performer whose job is to read the
/// stage directions out loud — so a rule that excluded them from matching would
/// silently drop a chunk of what is actually said on stage. Whether tracking should
/// stop is `hold`'s business, and it is stated per line rather than inferred.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    #[default]
    Dialogue,
    /// A stage direction (didascalie).
    Stage,
}

/// A passage where the script stops predicting what will be heard.
///
/// Music, a held silence, an improvised exchange: the show is running, the operator
/// still needs the page, and there is simply nothing to match against. Left to itself
/// the tracker treats the gap as evidence — the recogniser writes down the music, none
/// of it fits, the position's evidence erodes and something four hundred lines away
/// starts to look reasonable. Saying so in the script costs one field and removes the
/// guesswork.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hold {
    /// Nobody speaks.
    Silence,
    /// Music plays. Whisper will transcribe it as dialogue; it means nothing.
    Music,
    /// The company is off-script — improvisation, riffing, a running gag played long.
    Adlib,
}

/// Who left a bookmark on a line, and when.
///
/// Attributed to the **cue list**, not to a person. A list is a position — the sound
/// conduite, the LX plot — and "the sound flags" is the granularity anybody actually
/// asks in. It also means no identity plumbing: a screen already knows which list it is
/// showing, so a flag is attributed for free, and nothing is authorised by a name.
///
/// An array on the line, because two operators noticing the same bad line is
/// information rather than a duplicate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flag {
    /// The cue list's name, as its operator wrote it.
    pub by: String,
    /// ISO 8601, stamped by the client that placed it — a bookmark wants a wall clock,
    /// not a trusted one.
    pub at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptLine {
    pub id: String,
    pub act: String,
    pub scene: String,
    pub character: String,
    pub text: String,
    /// `None` means inherit (§8.2). An array with two entries is a bilingual line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<Vec<LangCode>>,
    /// Explicit landmark weight 1–3, or 0 for none (notation §7). Act and scene
    /// boundaries are implicit weight-3 landmarks and need no entry here.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub landmark: u8,
    /// Struck from this production, but kept in the script.
    ///
    /// Marked rather than deleted, on the notation spec's no-silent-data-loss rule: a
    /// line cut this week may be restored next week, and anything anchored to it has
    /// to survive in the meantime. The operator needs to *see* it — reading a page
    /// that contains text nobody is going to say is how you lose your place — so it
    /// stays in the display, visibly struck.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cut: bool,
    /// Ways this line has actually been performed, as distinct from how it is
    /// written.
    ///
    /// A production's text keeps moving after the premiere while the script
    /// usually does not, and on real touring material that drift was the single
    /// largest source of tracking loss. An alternate is an *additional* thing to
    /// match against; it never replaces `text`, which stays exactly as the
    /// playwright wrote it and is what every client displays (notation §2,
    /// principle 1: pristine text).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternates: Vec<String>,
    /// Dialogue, or a stage direction. See [`LineKind`].
    #[serde(default, skip_serializing_if = "is_dialogue")]
    pub kind: LineKind,
    /// Whether anyone says this out loud. `None` means "as the kind implies".
    ///
    /// Stated, never inferred, because the two are genuinely independent and this
    /// production proves it. *Hécube, pas Hécube* stages a play within a play, so its
    /// stage directions come from two works: Tiago Rodrigues' own, which describe what
    /// the company does and nobody voices, and Euripides', quoted in guillemets, which
    /// Séphora reads aloud as part of the performance — `SÉPHORA, lit les didascalies`
    /// is a line in the cast list.
    ///
    /// Checked against both recordings before deciding: the quoted directions score a
    /// mean 0.47 against the audio and the unquoted ones about 0.30, with
    /// `Hécube apparaît.` the one clean never-heard at 0.08 and 0.00. Suggestive,
    /// consistent with the guillemet rule, and nowhere near decisive — which is the
    /// argument for a field the operator sets rather than a heuristic that guesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoken: Option<bool>,
    /// Come back to this one.
    ///
    /// Not a property of the play and not an instruction to anything — a bookmark the
    /// operator drops while their attention is elsewhere. The moment you notice a line
    /// is wrong is almost never the moment you can stop and fix it: you are watching a
    /// run, or listening for a cue. Writing it down is the difference between fixing
    /// it tomorrow and rediscovering it at the next rehearsal.
    ///
    /// Kept in the script rather than in a browser tab for exactly that reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<Flag>,
    /// Tracking suspends here until something further on is heard. See [`Hold`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<Hold>,
    /// Roughly how long the hold lasts, for the display's benefit. A hint and never a
    /// gate: a company does not hit its music cue to the second, and a hold that
    /// expired on a stopwatch while the music was still playing would put back exactly
    /// the problem it was added to remove. The hold ends when the show is heard again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_seconds: Option<f64>,
}

fn is_dialogue(k: &LineKind) -> bool {
    matches!(k, LineKind::Dialogue)
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Script {
    #[serde(default = "script_format")]
    pub format: String,
    #[serde(default = "script_format_version")]
    pub format_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub default_lang: Vec<LangCode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acts: Vec<SectionMeta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenes: Vec<SectionMeta>,
    pub characters: Vec<Character>,
    pub lines: Vec<ScriptLine>,
}

fn script_format() -> String {
    "choufleur-script".to_string()
}
fn script_format_version() -> String {
    "0.1".to_string()
}

impl Script {
    pub fn character(&self, id: &str) -> Option<&Character> {
        self.characters.iter().find(|c| c.id == id)
    }

    /// Resolve a line's languages per §8.2.
    pub fn resolve_langs(&self, line: &ScriptLine) -> Vec<LangCode> {
        if let Some(l) = line.lang.as_ref().filter(|l| !l.is_empty()) {
            return l.clone();
        }
        if let Some(l) = self
            .character(&line.character)
            .and_then(|c| c.lang.as_ref())
            .filter(|l| !l.is_empty())
        {
            return l.clone();
        }
        for (sections, id) in [(&self.scenes, &line.scene), (&self.acts, &line.act)] {
            if let Some(l) = sections
                .iter()
                .find(|s| &s.id == id)
                .and_then(|s| s.lang.as_ref())
                .filter(|l| !l.is_empty())
            {
                return l.clone();
            }
        }
        self.default_lang.clone()
    }
}

/// One script line with everything the matcher needs precomputed.
#[derive(Clone, Debug)]
pub struct PreparedLine {
    pub index: usize,
    pub id: String,
    /// Struck from this production; see `ScriptLine::cut`.
    pub cut: bool,
    /// Dialogue or didascalie; see [`LineKind`].
    pub kind: LineKind,
    /// Who has bookmarked this line; see [`Flag`]. Never affects matching, and carried
    /// through so a client can be told without re-reading the script file.
    pub flags: Vec<Flag>,
    /// Someone says this out loud, so the matcher may look for it. Resolved from
    /// `ScriptLine::spoken`, or from the kind when that is unset.
    pub spoken: bool,
    /// Struck *or* unvoiced: either way the matcher must not look for it here.
    ///
    /// One predicate, because they are one situation. A cut line and a production
    /// stage direction are both text on the operator's page that will never come out
    /// of a loudspeaker, and every moment the matcher spends offering them as
    /// candidates is a moment it can be wrong for free.
    pub matchable: bool,
    /// Tracking suspends on this line; see [`Hold`].
    pub hold: Option<Hold>,
    pub hold_seconds: Option<f64>,
    /// Hash of the normalized text, so two lines that say the same thing can be
    /// recognised as saying the same thing without comparing strings in the hot path.
    ///
    /// A production repeats itself — a chorus, a running joke, a company reading the
    /// same passage aloud that they later perform. Those repetitions are not rival
    /// *answers*; they are one answer written down twice, and the matcher needs to
    /// know the difference.
    pub text_key: u64,
    pub character: String,
    pub act: String,
    pub scene: String,
    /// Pristine line text — used for ASR decode biasing, never for matching.
    pub text: String,
    /// Resolved languages (§8.2), each with every *form* of the line prepared
    /// under that language's normalizer: the written text first, then any
    /// alternates. A bilingual line carries two entries.
    pub by_lang: Vec<(LangCode, Vec<MatchText>)>,
    /// Explicit weight, or 3 for an act/scene opening line.
    pub landmark_weight: u8,
    pub is_section_start: bool,
}

impl PreparedLine {
    /// The written form under `lang` — the one used for span concatenation.
    pub fn match_text(&self, lang: &LangCode) -> Option<&MatchText> {
        self.forms(lang).and_then(|f| f.first())
    }

    /// Every form of this line under `lang`: written text first, then alternates.
    pub fn forms(&self, lang: &LangCode) -> Option<&[MatchText]> {
        self.by_lang
            .iter()
            .find(|(l, _)| l == lang)
            .map(|(_, m)| m.as_slice())
    }
    pub fn langs(&self) -> impl Iterator<Item = &LangCode> {
        self.by_lang.iter().map(|(l, _)| l)
    }
}

/// A contiguous-in-time run of script lines a single transcript segment may cover.
///
/// Members are line indices. For a per-character channel they are that character's
/// consecutive lines, which may be non-adjacent in the script (a short interjection
/// on another channel can sit between them); for a zone channel they are strictly
/// adjacent indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    members: [u32; MAX_SPAN],
    len: u8,
}

impl Span {
    pub fn single(index: usize) -> Self {
        let mut members = [0u32; MAX_SPAN];
        members[0] = index as u32;
        Span { members, len: 1 }
    }
    fn push(&mut self, index: usize) {
        self.members[self.len as usize] = index as u32;
        self.len += 1;
    }
    pub fn len(&self) -> usize {
        self.len as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn first(&self) -> usize {
        self.members[0] as usize
    }
    pub fn last(&self) -> usize {
        self.members[self.len as usize - 1] as usize
    }
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.members[..self.len as usize]
            .iter()
            .map(|&i| i as usize)
    }
}

/// The script, indexed for matching.
pub struct PreparedScript {
    pub lines: Vec<PreparedLine>,
    by_id: HashMap<String, usize>,
    /// Indices of lines opening an act or scene, ascending.
    section_starts: Vec<usize>,
    /// Every landmark index (explicit and implicit), ascending.
    landmarks: Vec<usize>,
    /// Group character id → the ids it is made of. Empty on almost every show.
    groups: HashMap<String, Vec<String>>,
}

impl PreparedScript {
    pub fn build(script: &Script, reg: &mut NormalizerRegistry) -> Self {
        let mut lines = Vec::with_capacity(script.lines.len());
        let mut by_id = HashMap::with_capacity(script.lines.len());
        let mut section_starts = Vec::new();
        let mut landmarks = Vec::new();
        let groups: HashMap<String, Vec<String>> = script
            .characters
            .iter()
            .filter(|c| !c.members.is_empty())
            .map(|c| (c.id.clone(), c.members.clone()))
            .collect();
        let (mut prev_act, mut prev_scene) = (None::<&str>, None::<&str>);

        for (index, line) in script.lines.iter().enumerate() {
            let is_section_start =
                prev_act != Some(line.act.as_str()) || prev_scene != Some(line.scene.as_str());
            prev_act = Some(&line.act);
            prev_scene = Some(&line.scene);

            let langs = script.resolve_langs(line);
            let by_lang = langs
                .iter()
                .map(|l| {
                    let mut forms = vec![reg.prepare(&line.text, l)];
                    forms.extend(line.alternates.iter().map(|a| reg.prepare(a, l)));
                    (l.clone(), forms)
                })
                .collect();

            let landmark_weight = if is_section_start {
                3
            } else {
                line.landmark.min(3)
            };
            if is_section_start {
                section_starts.push(index);
            }
            if landmark_weight > 0 {
                landmarks.push(index);
            }
            by_id.insert(line.id.clone(), index);
            let text_key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                crate::normalize::normalize_base(&line.text).hash(&mut h);
                h.finish()
            };
            lines.push(PreparedLine {
                index,
                id: line.id.clone(),
                cut: line.cut,
                kind: line.kind,
                flags: line.flags.clone(),
                spoken: line.spoken.unwrap_or(matches!(line.kind, LineKind::Dialogue)),
                // A hold is never matchable, whatever it was typed as.
                //
                // A hold declares that the script cannot predict what is about to come
                // out of the speakers. Offering it as a candidate contradicts that, and
                // the markers an operator inserts are short — "Musique", "Silence." —
                // which makes them the worst kind of candidate. Observed: a one-word
                // `Musique` line typed as dialogue matched at 0.84 on a segment merely
                // containing the word, in a play whose subject is a boy who loves Otis
                // Redding, and parked the tracker on a music hold for 110 seconds while
                // the company was speaking.
                //
                // So the position reaches a hold by being walked into it and by no
                // other route — see `Tracker::accept`.
                matchable: !line.cut
                    && line.hold.is_none()
                    && line.spoken.unwrap_or(matches!(line.kind, LineKind::Dialogue)),
                hold: line.hold,
                hold_seconds: line.hold_seconds,
                text_key,
                character: line.character.clone(),
                act: line.act.clone(),
                scene: line.scene.clone(),
                text: line.text.clone(),
                by_lang,
                landmark_weight,
                is_section_start,
            });
        }
        PreparedScript {
            lines,
            by_id,
            section_starts,
            landmarks,
            groups,
        }
    }

    /// Does a line owned by `line_character` come out of a mic labelled `heard`?
    ///
    /// Identity first, which is the answer on every show that has no groups and is a
    /// plain string comparison. The map is consulted only when the two names differ,
    /// and skipped entirely when nothing declared members — so a show without a chorus
    /// pays nothing for one, and the traces prove it: byte-identical on Hécube.
    pub fn speaks(&self, line_character: &str, heard: &str) -> bool {
        line_character == heard
            || (!self.groups.is_empty()
                && self
                    .groups
                    .get(line_character)
                    .is_some_and(|m| m.iter().any(|id| id == heard)))
    }

    /// The same question over everyone a channel might be carrying.
    ///
    /// A shared position mic offers two or three names and the line only has to be one
    /// of them. Empty means a zone channel, which agrees with everybody — the caller
    /// decides what that is worth, since a zone mic earns no confidence from agreeing.
    pub fn speaks_any(&self, line_character: &str, heard: &[String]) -> bool {
        heard.is_empty() || heard.iter().any(|h| self.speaks(line_character, h))
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
    pub fn index_of(&self, line_id: &str) -> Option<usize> {
        self.by_id.get(line_id).copied()
    }
    pub fn section_starts(&self) -> &[usize] {
        &self.section_starts
    }
    pub fn landmarks(&self) -> &[usize] {
        &self.landmarks
    }

    /// Landmark indices strictly after `from` and within `horizon` lines of it.
    pub fn landmarks_ahead(&self, from: usize, horizon: usize) -> &[usize] {
        let lo = self.landmarks.partition_point(|&i| i <= from);
        let hi = self
            .landmarks
            .partition_point(|&i| i <= from.saturating_add(horizon));
        &self.landmarks[lo..hi]
    }

    /// Candidate spans starting at or after `from`, within `window` lines.
    ///
    /// `speakers` is everyone the segment's channel might be carrying. Empty is a zone
    /// channel with no identity, which may match anyone; one name is the ordinary
    /// per-actor mic; several is a position mic two performers share.
    pub fn spans_into(
        &self,
        from: usize,
        window: usize,
        max_span: usize,
        speakers: &[String],
        out: &mut Vec<Span>,
    ) {
        out.clear();
        if self.lines.is_empty() {
            return;
        }
        let max_span = max_span.clamp(1, MAX_SPAN);
        let last_start = (from + window).min(self.lines.len() - 1);
        for start in from..=last_start {
            // Cut, or nobody says it: invisible to the matcher, though the operator
            // still sees it on the page. See `PreparedLine::matchable`.
            if !self.lines[start].matchable {
                continue;
            }
            if !self.speaks_any(&self.lines[start].character, speakers) {
                continue;
            }
            let mut span = Span::single(start);
            out.push(span);
            // Grow the span forward. A segment can only ever cover material the
            // audience heard back to back, so a span never crosses a scene break.
            let mut i = start;
            while span.len() < max_span {
                let Some(next) = self.next_member(i, start, speakers) else {
                    break;
                };
                if self.lines[next].scene != self.lines[start].scene {
                    break;
                }
                span.push(next);
                out.push(span);
                i = next;
            }
        }
    }

    fn next_member(&self, after: usize, start: usize, speakers: &[String]) -> Option<usize> {
        if speakers.is_empty() {
            // Zone channel: the next line the audience actually hears. A production
            // stage direction sitting between two spoken lines is not a gap in the
            // dialogue — nobody paused — so the span steps straight over it.
            return ((after + 1)..self.lines.len()).find(|&i| self.lines[i].matchable);
        }
        // Identified channel: the next line one of these people has, allowing a short
        // interjection on another channel to sit between them — but only a short one,
        // or a span could reach halfway across the scene.
        let limit = (start + MAX_SPAN * 2).min(self.lines.len().saturating_sub(1));
        ((after + 1)..=limit).find(|&i| {
            self.lines[i].matchable && self.speaks_any(&self.lines[i].character, speakers)
        })
    }

    /// Spans anchored on landmarks ahead of `from` — the re-anchoring candidates
    /// that stay live while the tracker is uncertain.
    pub fn landmark_spans_into(&self, from: usize, horizon: usize, out: &mut Vec<Span>) {
        out.clear();
        for &l in self.landmarks_ahead(from, horizon) {
            if !self.lines[l].matchable {
                continue;
            }
            let mut span = Span::single(l);
            out.push(span);
            if let Some(next) = self.next_member(l, l, &[]) {
                if self.lines[next].scene == self.lines[l].scene {
                    span.push(next);
                    out.push(span);
                }
            }
        }
    }

    /// Tokens of every member line of `span` under `lang`, concatenated.
    /// Returns `None` if any member lacks that language.
    pub fn span_tokens<'a>(&'a self, span: Span, lang: &LangCode) -> Option<Vec<&'a str>> {
        let mut out: Vec<&str> = Vec::new();
        for i in span.iter() {
            let mt = self.lines[i].match_text(lang)?;
            out.extend(mt.tokens.iter().map(String::as_str));
        }
        Some(out)
    }

    /// How many tokens a span offers to match against, in its first language.
    ///
    /// The size of the evidence a candidate *can* provide, as opposed to how well it
    /// happened to score. A one-word line scores well against almost anything.
    pub fn span_token_count(&self, span: Span) -> usize {
        span.iter()
            .filter_map(|i| {
                let line = self.lines.get(i)?;
                let (lang, _) = line.by_lang.first()?;
                line.match_text(lang).map(|mt| mt.tokens.len())
            })
            .sum()
    }

    /// Tokens of a single line under `lang`, written form only.
    pub fn line_tokens(&self, index: usize, lang: &LangCode) -> Option<Vec<&str>> {
        self.lines
            .get(index)?
            .match_text(lang)
            .map(|mt| mt.tokens.iter().map(String::as_str).collect())
    }

    /// Every form a span may be matched against under `lang`.
    ///
    /// A single-line span offers the written line and each alternate. Multi-line
    /// spans offer only the written concatenation: alternates exist because a
    /// *line* was reworded, and enumerating every combination across a span would
    /// be combinatorial for no benefit — a segment covering several lines is
    /// already matching loosely enough that one reworded member is absorbed.
    /// The span's text as sound, boundary-free and concatenated.
    ///
    /// Concatenated with nothing between the lines on purpose: the segment side has no
    /// boundaries either, and a recogniser that runs the end of one line into the
    /// start of the next — which is most of what a multi-line span exists to catch —
    /// produces exactly this.
    pub fn span_sound(&self, span: Span, lang: &LangCode) -> String {
        let mut out = String::new();
        for i in span.iter() {
            if let Some(mt) = self.lines[i].match_text(lang) {
                out.push_str(&mt.sound);
            }
        }
        out
    }

    pub fn span_token_variants<'a>(&'a self, span: Span, lang: &LangCode) -> Vec<Vec<&'a str>> {
        if span.len() == 1 {
            if let Some(forms) = self.lines[span.first()].forms(lang) {
                return forms
                    .iter()
                    .map(|mt| mt.tokens.iter().map(String::as_str).collect())
                    .collect();
            }
            return Vec::new();
        }
        self.span_tokens(span, lang).into_iter().collect::<Vec<_>>()
    }

    /// Languages appearing on any member of `span`, in first-seen order.
    /// Does any line carry an alternate? Used only for reporting.
    pub fn alternate_count(&self) -> usize {
        self.lines
            .iter()
            .map(|l| {
                l.by_lang
                    .first()
                    .map_or(0, |(_, f)| f.len().saturating_sub(1))
            })
            .sum()
    }

    pub fn span_langs(&self, span: Span) -> Vec<LangCode> {
        let mut out: Vec<LangCode> = Vec::new();
        for i in span.iter() {
            for l in self.lines[i].langs() {
                if !out.contains(l) {
                    out.push(l.clone());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn line(id: &str, act: &str, scene: &str, ch: &str, text: &str) -> ScriptLine {
        ScriptLine {
            cut: false,
            kind: LineKind::Dialogue,
            spoken: None,
            flags: Vec::new(),
            hold: None,
            hold_seconds: None,
            id: id.into(),
            act: act.into(),
            scene: scene.into(),
            character: ch.into(),
            text: text.into(),
            lang: None,
            landmark: 0,
            alternates: Vec::new(),
        }
    }

    pub(crate) fn toy() -> Script {
        Script {
            format: script_format(),
            format_version: script_format_version(),
            title: Some("Toy".into()),
            default_lang: vec![LangCode::new("fr")],
            acts: vec![],
            scenes: vec![],
            characters: vec![
                Character {
                    id: "char-a".into(),
                    name: "A".into(),
                    lang: None,
                    channels: vec![1],
                    members: Vec::new(),
                },
                Character {
                    id: "char-b".into(),
                    name: "B".into(),
                    lang: Some(vec![LangCode::new("en")]),
                    channels: vec![2],
                    members: Vec::new(),
                },
            ],
            lines: vec![
                line("L-0001", "act-1", "sc-1", "char-a", "Bonjour."),
                line("L-0002", "act-1", "sc-1", "char-b", "Hello there."),
                line("L-0003", "act-1", "sc-1", "char-a", "Alors pars."),
                line("L-0004", "act-1", "sc-1", "char-a", "Maintenant."),
                line("L-0005", "act-1", "sc-2", "char-b", "A new scene."),
            ],
        }
    }

    fn prepared(script: &Script) -> PreparedScript {
        let mut reg = NormalizerRegistry::with_defaults();
        PreparedScript::build(script, &mut reg)
    }

    #[test]
    fn language_inheritance_follows_8_2() {
        let mut s = toy();
        s.acts = vec![SectionMeta {
            id: "act-1".into(),
            title: None,
            lang: Some(vec![LangCode::new("sv")]),
        }];
        // character override beats act
        assert_eq!(s.resolve_langs(&s.lines[1]), vec![LangCode::new("en")]);
        // act override beats show default
        assert_eq!(s.resolve_langs(&s.lines[0]), vec![LangCode::new("sv")]);
        // line override beats everything
        s.lines[0].lang = Some(vec![LangCode::new("fr"), LangCode::new("en")]);
        assert_eq!(
            s.resolve_langs(&s.lines[0]),
            vec![LangCode::new("fr"), LangCode::new("en")]
        );
    }

    #[test]
    fn scene_and_act_openings_are_implicit_weight_3_landmarks() {
        let p = prepared(&toy());
        assert_eq!(p.section_starts(), &[0, 4]);
        assert_eq!(p.landmarks(), &[0, 4]);
        assert_eq!(p.lines[0].landmark_weight, 3);
        assert_eq!(p.lines[4].landmark_weight, 3);
        assert_eq!(p.lines[1].landmark_weight, 0);
    }

    #[test]
    fn explicit_landmarks_join_the_index() {
        let mut s = toy();
        s.lines[2].landmark = 2;
        let p = prepared(&s);
        assert_eq!(p.landmarks(), &[0, 2, 4]);
        assert_eq!(p.landmarks_ahead(0, 3), &[2]);
        assert_eq!(p.landmarks_ahead(0, 40), &[2, 4]);
        assert!(p.landmarks_ahead(4, 40).is_empty());
    }

    #[test]
    fn zone_spans_are_strictly_adjacent() {
        let p = prepared(&toy());
        let mut out = Vec::new();
        p.spans_into(0, 1, 3, &[], &mut out);
        let shapes: Vec<Vec<usize>> = out.iter().map(|s| s.iter().collect()).collect();
        assert_eq!(
            shapes,
            vec![
                vec![0],
                vec![0, 1],
                vec![0, 1, 2],
                vec![1],
                vec![1, 2],
                vec![1, 2, 3]
            ]
        );
    }

    #[test]
    fn character_spans_skip_the_other_channels_lines() {
        let p = prepared(&toy());
        let mut out = Vec::new();
        p.spans_into(0, 4, 3, &["char-a".to_string()], &mut out);
        let shapes: Vec<Vec<usize>> = out.iter().map(|s| s.iter().collect()).collect();
        // Line 1 belongs to char-b, so char-a's span 0 continues at line 2.
        assert!(shapes.contains(&vec![0, 2, 3]));
        assert!(shapes
            .iter()
            .all(|s| s.iter().all(|&i| p.lines[i].character == "char-a")));
    }

    #[test]
    fn spans_never_cross_a_scene_boundary() {
        let p = prepared(&toy());
        let mut out = Vec::new();
        p.spans_into(3, 2, 3, &[], &mut out);
        for span in &out {
            let scenes: Vec<&str> = span.iter().map(|i| p.lines[i].scene.as_str()).collect();
            assert!(
                scenes.windows(2).all(|w| w[0] == w[1]),
                "span crossed scenes: {scenes:?}"
            );
        }
    }

    #[test]
    fn span_tokens_concatenate_members() {
        let p = prepared(&toy());
        let mut out = Vec::new();
        p.spans_into(2, 0, 2, &["char-a".to_string()], &mut out);
        let two = out.iter().find(|s| s.len() == 2).unwrap();
        let toks = p.span_tokens(*two, &LangCode::new("fr")).unwrap();
        assert_eq!(toks, vec!["alors", "pars", "maintenant"]);
    }

    #[test]
    fn round_trips_through_json() {
        let s = toy();
        let json = serde_json::to_string(&s).unwrap();
        let back: Script = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lines.len(), s.lines.len());
        assert_eq!(back.lines[0].id, "L-0001");
        assert_eq!(back.default_lang, vec![LangCode::new("fr")]);
    }
}

#[cfg(test)]
mod group_tests {
    use super::tests::{line, toy};
    use super::*;

    fn script_with_chorus() -> Script {
        let mut s = toy();
        s.characters.push(Character {
            id: "char-le-choeur".into(),
            name: "LE CHŒUR".into(),
            lang: None,
            channels: vec![],
            // In *Hécube, pas Hécube* the chorus is Éric, Gaël, Élissa and Séphora —
            // a fact about the production that nothing can derive from the text.
            members: vec!["char-a".into(), "char-b".into()],
        });
        s.lines = vec![
            line("L-1", "act-1", "scene-1", "char-le-choeur", "Nous sommes le choeur."),
            line("L-2", "act-1", "scene-1", "char-a", "Et moi je suis seul."),
        ];
        s
    }

    #[test]
    fn a_chorus_line_is_heard_on_any_members_mic() {
        let prepared =
            PreparedScript::build(&script_with_chorus(), &mut NormalizerRegistry::with_defaults());
        assert!(prepared.speaks("char-le-choeur", "char-a"));
        assert!(prepared.speaks("char-le-choeur", "char-b"));
        // Not the other way round: A's own lines are his, not the chorus's.
        assert!(!prepared.speaks("char-a", "char-le-choeur"));
        // And nobody outside it.
        assert!(!prepared.speaks("char-le-choeur", "char-z"));
    }

    #[test]
    fn a_show_with_no_groups_compares_names_and_nothing_else() {
        let mut s = script_with_chorus();
        for c in &mut s.characters {
            c.members.clear();
        }
        let prepared = PreparedScript::build(&s, &mut NormalizerRegistry::with_defaults());
        assert!(prepared.speaks("char-a", "char-a"));
        assert!(!prepared.speaks("char-le-choeur", "char-a"));
    }

    #[test]
    fn a_chorus_line_is_a_candidate_for_a_members_channel() {
        let prepared =
            PreparedScript::build(&script_with_chorus(), &mut NormalizerRegistry::with_defaults());
        let mut spans = Vec::new();
        prepared.spans_into(0, prepared.len(), 1, &["char-a".to_string()], &mut spans);
        let starts: Vec<usize> = spans.iter().map(|s| s.first()).collect();
        assert!(starts.contains(&0), "the chorus line is reachable from A's mic");
        assert!(starts.contains(&1), "and so is his own");
    }
}
