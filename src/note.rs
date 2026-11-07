//! One note as text: frontmatter, headings, observations and relations.

use serde_json::Value;
use serde_yaml_ng::Mapping;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

#[derive(Debug, Clone)]
pub struct Observation {
    pub category: Option<String>,
    pub content: String,
    pub tags: Vec<String>,
    pub context: Option<String>,
    /// 1-based line of the file.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub rel_type: String,
    pub target: String,
    pub context: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Heading {
    pub level: usize,
    pub text: String,
    /// 0-based index into the file's lines.
    pub idx: usize,
}

#[derive(Debug, Clone)]
pub struct Note {
    pub frontmatter: Mapping,
    pub title: String,
    pub note_type: String,
    pub permalink: String,
    pub tags: Vec<String>,
    /// Text after the frontmatter.
    pub body: String,
    pub observations: Vec<Observation>,
    pub relations: Vec<Relation>,
}

/// Splits `---` frontmatter off. Returns the YAML, the body and the number
/// of file lines before the body.
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str, usize) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (None, text, 0);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(yaml), body, yaml.lines().count() + 2);
        }
        offset += line.len();
    }
    (None, text, 0)
}

pub fn parse_frontmatter(yaml: Option<&str>) -> Mapping {
    yaml.and_then(|y| serde_yaml_ng::from_str::<Mapping>(y).ok()).unwrap_or_default()
}

pub fn fm_str(fm: &Mapping, key: &str) -> Option<String> {
    match fm.get(key)? {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn fm_tags(fm: &Mapping) -> Vec<String> {
    match fm.get("tags") {
        Some(serde_yaml_ng::Value::Sequence(s)) => s.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(serde_yaml_ng::Value::String(s)) => split_tags(s),
        _ => vec![],
    }
}

pub fn split_tags(s: &str) -> Vec<String> {
    s.split(',').map(|t| t.trim().trim_start_matches('#').to_string()).filter(|t| !t.is_empty()).collect()
}

pub fn fm_json(fm: &Mapping) -> Value {
    serde_json::to_value(fm).unwrap_or(Value::Null)
}

/// Lowercase ASCII-ish slug: accents dropped, runs of anything else become
/// one `-`, and `/` separates segments.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfd().filter(|c| !is_combining_mark(*c)).flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if c == '/' {
            while out.ends_with('-') {
                out.pop();
            }
            if !out.is_empty() && !out.ends_with('/') {
                out.push('/');
            }
        } else if !out.is_empty() && !out.ends_with('-') && !out.ends_with('/') {
            out.push('-');
        }
    }
    while out.ends_with('-') || out.ends_with('/') {
        out.pop();
    }
    out
}

/// A file name for a title: characters file systems or links dislike become `-`.
pub fn file_stem_for_title(title: &str) -> String {
    title
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn default_permalink(project: &str, rel_path: &str) -> String {
    format!("{project}/{}", slugify(rel_path.strip_suffix(".md").unwrap_or(rel_path)))
}

pub fn parse(project: &str, rel_path: &str, text: &str) -> Note {
    let (yaml, body, body_offset) = split_frontmatter(text);
    let frontmatter = parse_frontmatter(yaml);
    let stem = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let stem = stem.strip_suffix(".md").unwrap_or(stem);
    let title = fm_str(&frontmatter, "title").unwrap_or_else(|| stem.to_string());
    let note_type = fm_str(&frontmatter, "type").unwrap_or_else(|| "note".into());
    let permalink = fm_str(&frontmatter, "permalink").unwrap_or_else(|| default_permalink(project, rel_path));
    let tags = fm_tags(&frontmatter);
    let (observations, relations) = scan_body(body, body_offset);
    Note { frontmatter, title, note_type, permalink, tags, body: body.to_string(), observations, relations }
}

/// Lines that are code (fenced blocks), so neither observations nor links.
fn code_mask(lines: &[&str]) -> Vec<bool> {
    let mut fence: Option<&str> = None;
    lines
        .iter()
        .map(|l| {
            let t = l.trim_start();
            match fence {
                Some(f) if t.starts_with(f) => {
                    fence = None;
                    true
                }
                Some(_) => true,
                None if t.starts_with("```") || t.starts_with("~~~") => {
                    fence = Some(&t[..3]);
                    true
                }
                None => false,
            }
        })
        .collect()
}

pub fn headings(lines: &[&str]) -> Vec<Heading> {
    let code = code_mask(lines);
    lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !code[*i])
        .filter_map(|(idx, l)| {
            let level = l.chars().take_while(|c| *c == '#').count();
            let rest = &l[level..];
            ((1..=6).contains(&level) && (rest.starts_with(' ') || rest.is_empty())).then(|| Heading {
                level,
                text: rest.trim().trim_end_matches('#').trim().to_string(),
                idx,
            })
        })
        .collect()
}

/// `- item`, `* item`, `+ item`, `1. item` or `1) item`: the indentation and
/// the text after the marker.
fn list_item(line: &str) -> Option<(usize, &str)> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ ")) {
        return Some((indent, rest.trim()));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    let rest = t.get(digits..)?;
    (1..=9).contains(&digits).then_some(())?;
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")).map(|r| (indent, r.trim()))
}

/// Whether `text` is one balanced `(...)` group and nothing more.
fn single_parenthesized(text: &str) -> bool {
    if !text.starts_with('(') {
        return false;
    }
    let mut depth = 0;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i == text.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

fn scan_body(body: &str, offset: usize) -> (Vec<Observation>, Vec<Relation>) {
    let lines: Vec<&str> = body.lines().collect();
    let code = code_mask(&lines);
    let mut observations = Vec::new();
    let mut relations: Vec<Relation> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if code[i] {
            i += 1;
            continue;
        }
        let line_no = offset + i + 1;
        let Some((indent, first)) = list_item(lines[i]) else {
            for target in wikilinks(lines[i]) {
                push_relation(&mut relations, "links_to", target, None, line_no);
            }
            i += 1;
            continue;
        };
        // A list item runs on over indented continuation lines.
        let mut text = first.to_string();
        let mut j = i + 1;
        while j < lines.len() && !code[j] {
            let l = lines[j];
            let ind = l.len() - l.trim_start().len();
            if l.trim().is_empty() || ind <= indent || list_item(l).is_some() || l.trim_start().starts_with('#') {
                break;
            }
            text.push(' ');
            text.push_str(l.trim());
            j += 1;
        }
        let explicit = explicit_relation(&text);
        if let Some((rel_type, target, context)) = &explicit {
            push_relation(&mut relations, rel_type, target.clone(), context.clone(), line_no);
        } else if let Some(obs) = observation(&text, line_no) {
            observations.push(obs);
        }
        for target in wikilinks(&text) {
            if explicit.as_ref().is_none_or(|(_, t, _)| *t != target) {
                push_relation(&mut relations, "links_to", target, None, line_no);
            }
        }
        i = j;
    }
    (observations, relations)
}

fn push_relation(relations: &mut Vec<Relation>, rel_type: &str, target: String, context: Option<String>, line: usize) {
    if target.is_empty() || relations.iter().any(|r| r.rel_type == rel_type && r.target == target) {
        return;
    }
    relations.push(Relation { rel_type: rel_type.to_string(), target, context, line });
}

/// Text with inline code blanked out, so `[[x]]` inside backticks is no link.
fn mask_inline_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_code = false;
    for c in s.chars() {
        if c == '`' {
            in_code = !in_code;
            out.push(c);
        } else if in_code && (c == '[' || c == ']') {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// Every `[[target]]` (or `[[target|label]]`) outside inline code.
pub fn wikilinks(s: &str) -> Vec<String> {
    let masked = mask_inline_code(s);
    let mut out = Vec::new();
    let mut rest = masked.as_str();
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let inner = &after[..end];
        let target = inner.split('|').next().unwrap_or("").trim();
        if !target.is_empty() && !target.contains('[') {
            out.push(target.to_string());
        }
        rest = &after[end + 2..];
    }
    out
}

/// `relation_type [[Target]] (context)`: a label without spaces, or a quoted one.
fn explicit_relation(text: &str) -> Option<(String, String, Option<String>)> {
    let masked = mask_inline_code(text);
    let start = masked.find("[[")?;
    let label = masked[..start].trim();
    if label.is_empty() || label.starts_with('[') {
        return None;
    }
    let label = match label.chars().next() {
        Some(q @ ('"' | '\'')) if label.len() > 1 && label.ends_with(q) => label[1..label.len() - 1].trim().to_string(),
        _ if label.chars().any(char::is_whitespace) => return None,
        _ => label.to_string(),
    };
    if label.is_empty() {
        return None;
    }
    let target = wikilinks(&masked[start..]).into_iter().next()?;
    let end = masked[start..].find("]]").map(|e| start + e + 2)?;
    // A prose tail makes the line a sentence with a link in it, not a relation.
    let tail = text.get(end..).unwrap_or("").trim();
    if !tail.is_empty() && !single_parenthesized(tail) {
        return None;
    }
    let context = tail.strip_prefix('(').and_then(|t| t.strip_suffix(')')).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    Some((label, target, context))
}

fn is_timestamp(s: &str) -> bool {
    let one = |p: &str| {
        let main = p.split(['.', ',']).next().unwrap_or("");
        let parts: Vec<&str> = main.split(':').collect();
        (2..=3).contains(&parts.len())
            && parts.iter().all(|x| !x.is_empty() && x.len() <= 3 && x.chars().all(|c| c.is_ascii_digit()))
            && parts[1..].iter().all(|x| x.len() == 2)
    };
    one(s) || s.split_once(" - ").is_some_and(|(a, b)| one(a.trim()) && one(b.trim()))
}

fn observation(text: &str, line: usize) -> Option<Observation> {
    if ["[ ]", "[x]", "[X]", "[-]"].iter().any(|m| text.starts_with(m)) {
        return None;
    }
    if text.starts_with("[[") && text.ends_with("]]") {
        return None;
    }
    let mut category = None;
    let mut content = text;
    if let Some(rest) = text.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        let cat = rest[..close].trim();
        let after = &rest[close + 1..];
        let valid = !cat.is_empty()
            && !cat.contains(['[', '(', ')'])
            && !is_timestamp(cat)
            && !(cat.chars().count() == 1 && !cat.chars().all(char::is_alphanumeric))
            && after.starts_with(char::is_whitespace)
            && !after.trim().is_empty();
        if after.starts_with('(') {
            return None; // [text](url)
        }
        if valid {
            category = Some(cat.to_string());
            content = after.trim();
        }
    }
    let tags = hashtags(content);
    if category.is_none() && tags.is_empty() {
        return None;
    }
    let mut content = content.to_string();
    let mut context = None;
    if content.ends_with(')')
        && let Some(open) = content.rfind('(')
    {
        let ctx = content[open + 1..content.len() - 1].trim().to_string();
        let before = content[..open].trim().to_string();
        if !before.is_empty() {
            context = Some(ctx).filter(|c| !c.is_empty());
            content = before;
        }
    }
    Some(Observation { category, content, tags, context, line })
}

/// `#tag` words, outside inline code; `#a#b` counts as two.
pub fn hashtags(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    for word in s.split_whitespace() {
        let ticks = word.matches('`').count();
        let starts_code = in_code;
        if ticks % 2 == 1 {
            in_code = !in_code;
        }
        if starts_code || ticks > 0 || !word.starts_with('#') {
            continue;
        }
        for t in word.split('#').map(|t| t.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')) {
            if !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit()) && !out.iter().any(|o| o == t) {
                out.push(t.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ntitle: 'Nomes: convenção'\ntype: note\npermalink: main/conv/nomes\ntags:\n- a\n- b\n---\n\n# Nomes\n\n\
- [critério] Curto e pronunciável #nome\n  em português (dito em 2026)\n- [ ] tarefa\n- [00:01] fala\n- part_of [[Convenções pessoais]]\n\
- relates_to [[Farmann]] (precedente)\nVer [[Outra nota|rótulo]] e `[[não é link]]`.\n```\n- [fato] dentro de código\n```\n- solto com #tag\n";

    #[test]
    fn parses_frontmatter_observations_and_relations() {
        let n = parse("main", "conv/Nomes- convenção.md", NOTE);
        assert_eq!(n.title, "Nomes: convenção");
        assert_eq!(n.permalink, "main/conv/nomes");
        assert_eq!(n.tags, ["a", "b"]);
        assert_eq!(n.observations.len(), 2);
        let o = &n.observations[0];
        assert_eq!(o.category.as_deref(), Some("critério"));
        assert_eq!(o.content, "Curto e pronunciável #nome em português");
        assert_eq!(o.context.as_deref(), Some("dito em 2026"));
        assert_eq!(o.tags, ["nome"]);
        assert_eq!(o.line, 12);
        assert_eq!(n.observations[1].category, None);
        let rels: Vec<(&str, &str)> = n.relations.iter().map(|r| (r.rel_type.as_str(), r.target.as_str())).collect();
        assert_eq!(rels, [("part_of", "Convenções pessoais"), ("relates_to", "Farmann"), ("links_to", "Outra nota")]);
        assert_eq!(n.relations[1].context.as_deref(), Some("precedente"));
    }

    #[test]
    fn ordered_items_and_prose_tails() {
        let n = parse(
            "main",
            "x.md",
            "1. [lacuna] falta exame\n2) [fato] outro\n- veja [[A]] e depois [[B]]\n- candidata: [[C]] — porque sim\n- part_of [[D]] (ctx) \n",
        );
        assert_eq!(n.observations.iter().map(|o| o.category.as_deref().unwrap_or("")).collect::<Vec<_>>(), ["lacuna", "fato"]);
        let rels: Vec<(&str, &str)> = n.relations.iter().map(|r| (r.rel_type.as_str(), r.target.as_str())).collect();
        assert_eq!(rels, [("links_to", "A"), ("links_to", "B"), ("links_to", "C"), ("part_of", "D")]);
    }

    #[test]
    fn slugs_are_ascii_permalinks() {
        assert_eq!(slugify("projetos/Stack `mcp` em ~-mcp — separando os servidores"), "projetos/stack-mcp-em-mcp-separando-os-servidores");
        assert_eq!(slugify("Preferência: commitar"), "preferencia-commitar");
        assert_eq!(default_permalink("main", "saude/Saúde — índice.md"), "main/saude/saude-indice");
    }

    #[test]
    fn frontmatter_split_counts_lines() {
        let (y, b, off) = split_frontmatter("---\na: 1\n---\nbody\n");
        assert_eq!(y, Some("a: 1\n"));
        assert_eq!(b, "body\n");
        assert_eq!(off, 3);
        assert_eq!(split_frontmatter("no fm").2, 0);
    }
}
