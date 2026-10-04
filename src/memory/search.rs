//! Deterministic lexical recall. Scores describe relevance, never evidence
//! validity; callers must retain status and revalidate sources before use.
use super::{Memory, MemoryMeta, MemoryStatus, MemoryStore};
use caseless::Caseless;
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization;

fn normalize(text: &str) -> String {
    text.nfkc().default_case_fold().nfkc().collect()
}

/// Split snake/kebab/path separators and camelCase, including HTTPServer.
/// Normalize before splitting to handle full-width identifiers consistently,
/// but fold case afterwards so identifier boundaries are still available.
fn terms(text: &str) -> BTreeSet<String> {
    let chars: Vec<_> = text.nfkc().collect();
    let mut word = String::new();
    let mut words = BTreeSet::new();
    for (i, &ch) in chars.iter().enumerate() {
        let boundary = ch.is_uppercase()
            && i > 0
            && (chars[i - 1].is_lowercase()
                || chars[i - 1].is_numeric()
                || (chars[i - 1].is_uppercase()
                    && chars.get(i + 1).is_some_and(|next| next.is_lowercase())));
        if (!ch.is_alphanumeric() || boundary) && !word.is_empty() {
            words.insert(normalize(&word));
            word.clear();
        }
        if ch.is_alphanumeric() {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        words.insert(normalize(&word));
    }
    words
}

struct Query {
    literal: String,
    identity: String,
    terms: BTreeSet<String>,
}

impl Query {
    fn new(text: &str) -> Self {
        Self {
            // Keys are stored verbatim: " auth " and "auth" can name two
            // different memories. Whitespace-only queries still list all,
            // but identity matching must not silently address another key.
            literal: text.to_owned(),
            identity: normalize(text),
            terms: terms(text),
        }
    }

    fn exact(&self, memory: &Memory) -> usize {
        if self.literal.trim().is_empty() {
            0
        } else if self.literal == memory.id || memory.key.as_deref() == Some(&self.literal) {
            2
        } else if !self.identity.is_empty()
            && (self.identity == normalize(&memory.id)
                || memory
                    .key
                    .as_ref()
                    .is_some_and(|key| self.identity == normalize(key)))
        {
            1
        } else {
            0
        }
    }

    // Normalize each query independently. Repeated words and long to-dos
    // cannot increase the score simply by contributing more query tokens.
    fn score(&self, fields: &[Field]) -> usize {
        if self.terms.is_empty() {
            return 0;
        }
        let mut coverage = 0;
        let mut importance = 0;
        for term in &self.terms {
            let mut strength = 0;
            let mut weighted = 0;
            for field in fields {
                let matched = field.matches(term);
                strength = strength.max(matched);
                weighted = weighted.max(matched * field.weight);
            }
            coverage += strength;
            importance += weighted;
        }
        // Coverage dominates field weight: matching all requested concepts
        // in a body beats matching just one in a title. Partial matches earn
        // only a quarter of a complete token match.
        let score = coverage * 250 / self.terms.len() + importance * 10 / self.terms.len();
        if coverage > 0 { score.max(1) } else { 0 }
    }
}

struct Field {
    text: String,
    terms: BTreeSet<String>,
    weight: usize,
}

impl Field {
    fn new(text: &str, weight: usize) -> Self {
        let normalized = normalize(text);
        let mut words = terms(text);
        // Preserve the complete case-folded token as well as its camel-case
        // parts. "memoryread" must be a full title match for "memoryRead",
        // not a weaker substring match that loses to an unrelated body hit.
        words.extend(
            normalized
                .split(|ch: char| !ch.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_owned),
        );
        Self {
            text: normalized,
            terms: words,
            weight,
        }
    }

    fn matches(&self, term: &str) -> usize {
        if self.terms.contains(term) {
            4
        } else if (term.chars().count() >= 3
            || (term.chars().count() >= 2 && term.chars().all(|ch| ('가'..='힣').contains(&ch))))
            && self.text.contains(term)
        {
            // Keep literal partial search (including Korean suffixes), but
            // avoid one-character noise and short ASCII substring matches.
            1
        } else {
            0
        }
    }
}

pub(super) fn rank(
    store: &MemoryStore,
    request: &str,
    current_work: &str,
    tags: &[String],
    automatic: bool,
) -> Vec<MemoryMeta> {
    let list_all = !automatic && request.trim().is_empty();
    let request = Query::new(request);
    let work = Query::new(current_work);
    let mut matches = Vec::new();
    for memory in store.entries.values() {
        // Explicit searches can still discover every status, including a
        // superseded ID/key. Tag filters keep their existing exact semantics.
        if !tags.iter().all(|tag| memory.tags.contains(tag))
            || (automatic && memory.status == MemoryStatus::Superseded)
        {
            continue;
        }
        // A plan item that happens to equal a key must not outrank the user's
        // different question. Work identities break ties only with no request.
        let exact = if request.literal.trim().is_empty() {
            work.exact(memory)
        } else {
            request.exact(memory)
        };
        let score = if list_all {
            1
        } else {
            let paths = memory
                .sources
                .iter()
                .filter_map(|source| source.path.as_deref())
                .collect::<Vec<_>>()
                .join(" ");
            let fields = [
                Field::new(memory.key.as_deref().unwrap_or(""), 12),
                Field::new(&memory.title, 10),
                Field::new(&memory.tags.join(" "), 10),
                Field::new(&memory.summary, 6),
                Field::new(&paths, 3),
                Field::new(&memory.body, 2),
            ];
            let relevance = request.score(&fields) * 3 + work.score(&fields);
            let reliability = match memory.status {
                MemoryStatus::Active => 100,
                MemoryStatus::NeedsReview => 70,
                MemoryStatus::Superseded => 25,
            };
            relevance * reliability
        };
        if score > 0 || exact > 0 {
            matches.push((exact, score, memory));
        }
    }
    matches.sort_by(|(ea, sa, a), (eb, sb, b)| {
        eb.cmp(ea)
            .then(sb.cmp(sa))
            .then(b.updated_at.cmp(&a.updated_at))
            .then(a.id.cmp(&b.id))
    });
    matches.into_iter().map(|(_, _, m)| m.meta()).collect()
}
