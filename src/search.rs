//! Ranked search over notes, observations and relations: full text (FTS5,
//! BM25), meaning (cosine over chunk embeddings) and both fused by
//! reciprocal rank, plus the filters every mode shares.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use serde_json::Value;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::embed::{Entry, dot};
use crate::store::{NoteRow, Store};

/// Reciprocal rank fusion constant; 60 is the usual choice.
const RRF_K: f64 = 60.0;
/// Candidates each list contributes to a fusion.
const POOL: usize = 60;
/// Below this cosine a chunk is not a semantic match (bge-m3 scale).
pub const DEFAULT_MIN_SIMILARITY: f32 = 0.35;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Hybrid,
    Text,
    Semantic,
    Title,
    Permalink,
}

impl Mode {
    pub fn parse(s: Option<&str>, semantic_available: bool) -> Result<Self> {
        Ok(match s {
            None => {
                if semantic_available {
                    Mode::Hybrid
                } else {
                    Mode::Text
                }
            }
            Some("hybrid") => Mode::Hybrid,
            Some("text") => Mode::Text,
            Some("semantic") => Mode::Semantic,
            Some("title") => Mode::Title,
            Some("permalink") => Mode::Permalink,
            Some(other) => bail!("unknown search mode {other:?}"),
        })
    }
}

/// Accent-free lowercase, for matching that ignores diacritics like FTS does.
pub fn fold(s: &str) -> String {
    s.nfd().filter(|c| !is_combining_mark(*c)).flat_map(char::to_lowercase).collect()
}

#[derive(Debug, PartialEq)]
pub struct Query {
    /// FTS5 expression with every term required.
    pub all: Option<String>,
    /// Same terms, any of them, for when requiring all finds nothing.
    pub any: Option<String>,
    pub tags: Vec<String>,
    /// Plain words, for line matching.
    pub words: Vec<String>,
}

/// Quoted phrases, AND/OR/NOT and tag:name in; a safe FTS5 expression out.
pub fn parse_query(q: &str) -> Query {
    let mut parts: Vec<String> = Vec::new();
    let mut tags = Vec::new();
    let mut words = Vec::new();
    let mut explicit = false;
    let mut chars = q.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let phrase: String = chars.by_ref().take_while(|c| *c != '"').collect();
            let clean = phrase.replace('"', "");
            if !clean.trim().is_empty() {
                words.extend(clean.split_whitespace().map(fold));
                parts.push(format!("\"{}\"", clean.trim()));
            }
            continue;
        }
        let word: String = std::iter::from_fn(|| chars.next_if(|c| !c.is_whitespace())).collect();
        match word.as_str() {
            "AND" | "OR" | "NOT" => {
                explicit = true;
                parts.push(word);
            }
            _ => {
                if let Some(tag) = word.strip_prefix("tag:").or_else(|| word.strip_prefix('#')) {
                    if !tag.is_empty() {
                        tags.push(tag.to_string());
                    }
                    continue;
                }
                let clean: String = word.chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')).collect();
                let clean = clean.trim_matches(|c: char| !c.is_alphanumeric());
                if clean.is_empty() {
                    continue;
                }
                words.push(fold(clean));
                let star = if clean.chars().count() >= 3 { "*" } else { "" };
                parts.push(format!("\"{clean}\"{star}"));
            }
        }
    }
    while matches!(parts.last().map(String::as_str), Some("AND" | "OR" | "NOT")) {
        parts.pop();
    }
    while matches!(parts.first().map(String::as_str), Some("AND" | "OR")) {
        parts.remove(0);
    }
    let terms: Vec<&String> = parts.iter().filter(|p| !matches!(p.as_str(), "AND" | "OR" | "NOT")).collect();
    let all = (!terms.is_empty()).then(|| parts.join(" "));
    let any = (!explicit && terms.len() > 1).then(|| terms.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" OR "));
    Query { all, any, tags, words }
}

/// '7d', '24h', '2 weeks', '3 days ago', 'last week', 'yesterday' or a date,
/// as a Unix time. Windows shorter than a day widen to a day.
pub fn parse_since(s: &str, now: i64) -> Result<i64> {
    let t = s.trim().to_lowercase();
    let day = 86_400;
    let named = match t.as_str() {
        "today" => Some(day),
        "yesterday" => Some(2 * day),
        "last week" | "this week" => Some(7 * day),
        "last month" | "this month" => Some(30 * day),
        "last year" | "this year" => Some(365 * day),
        _ => None,
    };
    if let Some(span) = named {
        return Ok(now - span);
    }
    let body = t.trim_suffix(" ago").trim();
    let split = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
    if split > 0 {
        let n: i64 = body[..split].parse()?;
        let unit = body[split..].trim();
        let secs = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => day,
            "w" | "week" | "weeks" => 7 * day,
            "mo" | "month" | "months" => 30 * day,
            "y" | "year" | "years" => 365 * day,
            _ => -1,
        };
        if secs > 0 {
            return Ok(now - (n * secs).max(day));
        }
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s.trim()) {
        return Ok(d.timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).map(|d| d.and_utc().timestamp()).unwrap_or(0));
    }
    bail!("cannot read since={s:?}; use '7d', '24h', '2 days ago', 'last week' or a date")
}

fn dot_get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |v, k| v.get(k))
}

fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Number(x), Value::String(y)) | (Value::String(y), Value::Number(x)) => {
            let ord = x.as_f64()?.partial_cmp(&y.parse::<f64>().ok()?)?;
            Some(if matches!(a, Value::Number(_)) { ord } else { ord.reverse() })
        }
        _ => None,
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.eq_ignore_ascii_case(y),
        (Value::Array(xs), y) if !y.is_array() => xs.iter().any(|x| loose_eq(x, y)),
        _ => a == b || cmp_values(a, b) == Some(std::cmp::Ordering::Equal),
    }
}

/// Frontmatter filters: `{field: value | null | [all of] | {$op: ...}}`.
pub fn matches_filters(fm: &Value, filters: &serde_json::Map<String, Value>) -> Result<bool> {
    for (field, cond) in filters {
        let v = dot_get(fm, field);
        let ok = match cond {
            Value::Null => v.is_none_or(Value::is_null),
            Value::Array(want) => match v {
                Some(Value::Array(have)) => want.iter().all(|w| have.iter().any(|h| loose_eq(h, w))),
                Some(h) => want.len() == 1 && loose_eq(h, &want[0]),
                None => false,
            },
            Value::Object(ops) if ops.keys().all(|k| k.starts_with('$')) => {
                let mut ok = true;
                for (op, arg) in ops {
                    ok &= match (op.as_str(), v) {
                        ("$exists", v) => arg.as_bool().unwrap_or(true) == v.is_some_and(|v| !v.is_null()),
                        (_, None) => false,
                        ("$gt", Some(v)) => cmp_values(v, arg) == Some(std::cmp::Ordering::Greater),
                        ("$gte", Some(v)) => matches!(cmp_values(v, arg), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)),
                        ("$lt", Some(v)) => cmp_values(v, arg) == Some(std::cmp::Ordering::Less),
                        ("$lte", Some(v)) => matches!(cmp_values(v, arg), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)),
                        ("$in", Some(v)) => arg.as_array().is_some_and(|a| a.iter().any(|x| loose_eq(v, x))),
                        ("$between", Some(v)) => match arg.as_array().map(Vec::as_slice) {
                            Some([lo, hi]) => {
                                !matches!(cmp_values(v, lo), Some(std::cmp::Ordering::Less) | None)
                                    && !matches!(cmp_values(v, hi), Some(std::cmp::Ordering::Greater) | None)
                            }
                            _ => bail!("$between takes [low, high]"),
                        },
                        ("$contains", Some(v)) => match v {
                            Value::Array(a) => a.iter().any(|x| loose_eq(x, arg)),
                            Value::String(s) => arg.as_str().is_some_and(|a| fold(s).contains(&fold(a))),
                            _ => false,
                        },
                        (other, _) => bail!("unknown filter operator {other}"),
                    };
                }
                ok
            }
            want => v.is_some_and(|h| loose_eq(h, want)),
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Which notes a search may return; `None` fields do not restrict.
#[derive(Default)]
pub struct Scope {
    pub path: Option<String>,
    pub note_types: Option<Vec<String>>,
    pub since: Option<i64>,
    pub filters: Option<serde_json::Map<String, Value>>,
    pub tags: Vec<String>,
    pub only: Option<i64>,
}

impl Scope {
    pub fn is_open(&self) -> bool {
        self.path.is_none()
            && self.note_types.is_none()
            && self.since.is_none()
            && self.filters.is_none()
            && self.tags.is_empty()
            && self.only.is_none()
    }

    pub fn admits(&self, n: &NoteRow) -> Result<bool> {
        if self.only.is_some_and(|id| id != n.id) {
            return Ok(false);
        }
        if let Some(p) = &self.path {
            let p = p.trim_matches('/');
            if !p.is_empty() && !(n.path.starts_with(&format!("{p}/")) || n.path == p) {
                return Ok(false);
            }
        }
        if let Some(t) = &self.note_types
            && !t.iter().any(|t| t.eq_ignore_ascii_case(&n.note_type))
        {
            return Ok(false);
        }
        if self.since.is_some_and(|s| n.updated < s) {
            return Ok(false);
        }
        if !self.tags.iter().all(|t| n.tags.iter().any(|h| fold(h) == fold(t))) {
            return Ok(false);
        }
        match &self.filters {
            Some(f) => matches_filters(&n.frontmatter, f),
            None => Ok(true),
        }
    }

    /// The admitted note ids, or None when every note is admitted.
    pub fn allowed(&self, store: &Store) -> Result<Option<HashSet<i64>>> {
        if self.is_open() {
            return Ok(None);
        }
        let mut out = HashSet::new();
        for n in store.all_notes()? {
            if self.admits(&n)? {
                out.insert(n.id);
            }
        }
        Ok(Some(out))
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub id: i64,
    pub score: f64,
    pub excerpt: Option<String>,
    /// Chunk that matched by meaning, for an excerpt.
    pub chunk: Option<i64>,
}

fn rrf(lists: &[Vec<Hit>]) -> Vec<Hit> {
    let mut fused: HashMap<i64, Hit> = HashMap::new();
    for list in lists {
        for (rank, h) in list.iter().enumerate() {
            let e = fused.entry(h.id).or_insert_with(|| Hit { id: h.id, score: 0.0, excerpt: None, chunk: None });
            e.score += 1.0 / (RRF_K + 1.0 + rank as f64);
            if e.excerpt.is_none() {
                e.excerpt.clone_from(&h.excerpt);
            }
            if e.chunk.is_none() {
                e.chunk = h.chunk;
            }
        }
    }
    let mut out: Vec<Hit> = fused.into_values().collect();
    // Scaled so a result first in both lists scores 1.0.
    for h in &mut out {
        h.score *= (RRF_K + 1.0) / lists.len().max(1) as f64;
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    out
}

/// Text hits, and whether every term matched. Any term is tried when requiring all finds nothing.
pub fn text_notes(store: &Store, q: &Query, allowed: &Option<HashSet<i64>>, limit: usize) -> Result<(Vec<Hit>, bool)> {
    let run = |expr: &str| -> Result<Vec<Hit>> {
        let rows = store.fts_notes(expr, if allowed.is_some() { 5000 } else { limit })?;
        Ok(rows
            .into_iter()
            .filter(|(id, _, _)| allowed.as_ref().is_none_or(|a| a.contains(id)))
            .take(limit)
            .map(|(id, rank, ex)| Hit { id, score: -rank, excerpt: Some(ex), chunk: None })
            .collect())
    };
    let Some(all) = &q.all else { return Ok((vec![], true)) };
    let hits = run(all)?;
    match (&q.any, hits.is_empty()) {
        (Some(any), true) => Ok((run(any)?, false)),
        _ => Ok((hits, true)),
    }
}

/// Text hits per note by its best passage, as `text_notes` returns them. Requiring every term
/// inside one passage keeps a long note from matching a question by its common words alone.
pub fn text_chunks(store: &Store, q: &Query, allowed: &Option<HashSet<i64>>, limit: usize) -> Result<(Vec<Hit>, bool)> {
    let run = |expr: &str| -> Result<Vec<Hit>> {
        let mut seen = HashSet::new();
        Ok(store
            .fts_chunks(expr, if allowed.is_some() { 20_000 } else { limit * 8 })?
            .into_iter()
            .filter(|(id, ..)| allowed.as_ref().is_none_or(|a| a.contains(id)) && seen.insert(*id))
            .take(limit)
            .map(|(id, chunk, rank, ex)| Hit { id, score: -rank, excerpt: Some(ex), chunk: Some(chunk) })
            .collect())
    };
    let Some(all) = &q.all else { return Ok((vec![], true)) };
    let hits = run(all)?;
    match (&q.any, hits.is_empty()) {
        (Some(any), true) => Ok((run(any)?, false)),
        _ => Ok((hits, true)),
    }
}

pub fn text_observations(store: &Store, q: &Query, limit: usize) -> Result<(Vec<Hit>, bool)> {
    let Some(all) = &q.all else { return Ok((vec![], true)) };
    let mut rows = store.fts_observations(all, limit)?;
    let mut strict = true;
    if rows.is_empty()
        && let Some(any) = &q.any
    {
        rows = store.fts_observations(any, limit)?;
        strict = false;
    }
    Ok((rows.into_iter().map(|(id, rank)| Hit { id, score: -rank, excerpt: None, chunk: None }).collect(), strict))
}

/// How much of its score a demoted observation keeps: enough to rank after a current one that matches as well.
pub const DEMOTE: f64 = 0.95;

/// Scales down the hits that are demoted observations, then re-sorts.
pub fn demote(mut hits: Vec<Hit>, demoted: &HashSet<i64>) -> Vec<Hit> {
    if demoted.is_empty() {
        return hits;
    }
    for h in hits.iter_mut().filter(|h| demoted.contains(&h.id)) {
        h.score *= DEMOTE;
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    hits
}

/// Meaning hits per note (best chunk) or per observation. A demoted observation's chunk counts for less.
pub fn semantic(
    entries: &[Entry],
    qv: &[f32],
    observations: bool,
    allowed: &Option<HashSet<i64>>,
    demoted: &HashSet<i64>,
    min_sim: f32,
    limit: usize,
) -> Vec<Hit> {
    let mut best: HashMap<i64, (f32, i64)> = HashMap::new();
    for e in entries {
        if allowed.as_ref().is_some_and(|a| !a.contains(&e.note_id)) {
            continue;
        }
        let key = match (observations, e.obs_id) {
            (true, Some(o)) => o,
            (true, None) => continue,
            (false, _) => e.note_id,
        };
        let s = dot(&e.vec, qv);
        if s < min_sim {
            continue;
        }
        let s = if e.obs_id.is_some_and(|o| demoted.contains(&o)) { s * DEMOTE as f32 } else { s };
        let slot = best.entry(key).or_insert((f32::MIN, e.chunk_id));
        if s > slot.0 {
            *slot = (s, e.chunk_id);
        }
    }
    let mut out: Vec<Hit> = best.into_iter().map(|(id, (s, c))| Hit { id, score: s as f64, excerpt: None, chunk: Some(c) }).collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    out.truncate(limit);
    out
}

/// Text hits that matched only some of the terms are noise next to meaning hits on a long
/// question, so they join the fusion only when every term matched (or nothing matched by meaning).
pub fn fuse((text, strict): (Vec<Hit>, bool), sem: Vec<Hit>) -> Vec<Hit> {
    match (text.is_empty() || !strict, sem.is_empty()) {
        (_, true) => rrf(&[text]),
        (true, _) => rrf(&[sem]),
        _ => rrf(&[text.into_iter().take(POOL).collect(), sem.into_iter().take(POOL).collect()]),
    }
}

pub fn pool() -> usize {
    POOL
}

/// Glob with `*` and `?`, case-insensitive.
pub fn glob(pattern: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) => a == b && go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = fold(pattern).chars().collect();
    let s: Vec<char> = fold(s).chars().collect();
    go(&p, &s)
}

/// Lines of `text` holding any of `words`, each with `context` lines
/// around, as (1-based first line, block) pairs; at most `max` matches.
/// Frontmatter lines never match.
pub fn grep(text: &str, words: &[String], context: usize, max: usize) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let skip = crate::note::split_frontmatter(text).2;
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .skip(skip)
        .filter(|(_, l)| words.iter().any(|w| fold(l).contains(w.as_str())))
        .map(|(i, _)| i)
        .take(max)
        .collect();
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for i in hits {
        let (a, b) = (i.saturating_sub(context), (i + context).min(lines.len().saturating_sub(1)));
        match blocks.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = b,
            _ => blocks.push((a, b)),
        }
    }
    blocks.into_iter().map(|(a, b)| (a + 1, lines[a..=b].join("\n"))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn queries_become_safe_fts() {
        let q = parse_query("busca \"frase exata\" tag:infra NOT velho o*");
        assert_eq!(q.all.as_deref(), Some("\"busca\"* \"frase exata\" NOT \"velho\"* \"o\""));
        assert_eq!(q.any, None);
        assert_eq!(q.tags, ["infra"]);
        let q = parse_query("quanto dinheiro sobra?");
        assert_eq!(q.any.as_deref(), Some("\"quanto\"* OR \"dinheiro\"* OR \"sobra\"*"));
        assert_eq!(parse_query("  AND ").all, None);
    }

    #[test]
    fn since_forms() {
        let now = 10_000_000;
        assert_eq!(parse_since("7d", now).unwrap(), now - 7 * 86_400);
        assert_eq!(parse_since("2 hours ago", now).unwrap(), now - 86_400);
        assert_eq!(parse_since("last week", now).unwrap(), now - 7 * 86_400);
        assert_eq!(parse_since("1970-01-02", now).unwrap(), 86_400);
        assert!(parse_since("whenever", now).is_err());
    }

    #[test]
    fn filters() {
        let fm = json!({"status": "Active", "tags": ["a", "b"], "priority": 3, "schema": {"confidence": 0.9}});
        let f = |v: Value| matches_filters(&fm, v.as_object().unwrap()).unwrap();
        assert!(f(json!({"status": "active"})));
        assert!(f(json!({"tags": ["a"]})));
        assert!(!f(json!({"tags": ["a", "c"]})));
        assert!(f(json!({"owner": null})));
        assert!(f(json!({"priority": {"$gte": 3, "$lt": 4}})));
        assert!(f(json!({"priority": {"$in": [1, 3]}})));
        assert!(f(json!({"schema.confidence": {"$between": [0.5, 1]}})));
        assert!(f(json!({"owner": {"$exists": false}})));
        assert!(!f(json!({"status": {"$contains": "x"}})));
    }

    #[test]
    fn demoted_hits_rank_after_equal_ones() {
        let hit = |id, score| Hit { id, score, excerpt: None, chunk: None };
        let out = demote(vec![hit(1, 1.0), hit(2, 0.99), hit(3, 0.5)], &HashSet::from([1]));
        assert_eq!(out.iter().map(|h| h.id).collect::<Vec<_>>(), [2, 1, 3]);
    }

    #[test]
    fn fusion_rewards_agreement() {
        let h = |id| Hit { id, score: 0.0, excerpt: None, chunk: None };
        let out = fuse((vec![h(1), h(2), h(3)], true), vec![h(2), h(4)]);
        assert_eq!(out[0].id, 2);
        // A text list that matched only some terms stays out.
        let loose = fuse((vec![h(5), h(6)], false), vec![h(6), h(8)]);
        assert_eq!(loose.iter().map(|h| h.id).collect::<Vec<_>>(), [6, 8]);
        let solo = fuse((vec![h(7)], true), vec![]);
        assert!((solo[0].score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn glob_and_grep() {
        assert!(glob("*reunião*", "notas/Reuniao de equipe.md"));
        assert!(!glob("*.txt", "a.md"));
        let g = grep("a\nb alvo\nc\nd\ne\nf alvo\n", &["alvo".into()], 1, 10);
        assert_eq!(g, [(1, "a\nb alvo\nc".to_string()), (5, "e\nf alvo".to_string())]);
        assert_eq!(grep("---\ntitle: alvo\n---\nalvo\n", &["alvo".into()], 0, 10), [(4, "alvo".to_string())]);
    }
}
