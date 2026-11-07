//! Text operations on notes: composing frontmatter, the mem_edit commands,
//! slicing a read, and writing files so a reader never sees half of one.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use serde_yaml_ng::Mapping;

use crate::note::{self, Heading};

/// Writes through a temporary file and a rename.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("note path has no folder")?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Unique per write, so two writes to one note never share a temporary file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}-{seq}.muninn-tmp", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

pub fn yaml_value(v: &Value) -> serde_yaml_ng::Value {
    serde_yaml_ng::to_value(v).unwrap_or(serde_yaml_ng::Value::Null)
}

/// Merges `metadata` into a frontmatter mapping, keeping key order.
pub fn merge_metadata(fm: &mut Mapping, metadata: &serde_json::Map<String, Value>) {
    for (k, v) in metadata {
        fm.insert(serde_yaml_ng::Value::String(k.clone()), yaml_value(v));
    }
}

pub fn render(fm: &Mapping, body: &str) -> Result<String> {
    if fm.is_empty() {
        return Ok(body.to_string());
    }
    let yaml = serde_yaml_ng::to_string(fm)?;
    let body = body.strip_prefix('\n').unwrap_or(body);
    Ok(format!("---\n{yaml}---\n\n{body}"))
}

/// Replaces a note's frontmatter, leaving its body byte for byte.
pub fn with_frontmatter(text: &str, fm: &Mapping) -> Result<String> {
    let (_, body, _) = note::split_frontmatter(text);
    let yaml = serde_yaml_ng::to_string(fm)?;
    Ok(format!("---\n{yaml}---\n{body}"))
}

pub fn frontmatter_of(text: &str) -> Mapping {
    note::parse_frontmatter(note::split_frontmatter(text).0)
}

fn key(k: &str) -> serde_yaml_ng::Value {
    serde_yaml_ng::Value::String(k.to_string())
}

/// The frontmatter of a new note: what the content brings, then title, type,
/// permalink and tags where it says nothing, then the caller's metadata.
pub fn compose_new(
    content: &str,
    title: &str,
    note_type: &str,
    permalink: &str,
    tags: &[String],
    metadata: Option<&serde_json::Map<String, Value>>,
) -> Result<String> {
    let (yaml, body, _) = note::split_frontmatter(content);
    let given = note::parse_frontmatter(yaml);
    let mut fm = Mapping::new();
    fm.insert(key("title"), given.get("title").cloned().unwrap_or_else(|| key(title)));
    fm.insert(key("type"), given.get("type").cloned().unwrap_or_else(|| key(note_type)));
    fm.insert(key("permalink"), given.get("permalink").cloned().unwrap_or_else(|| key(permalink)));
    let mut all_tags = note::fm_tags(&given);
    for t in tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }
    if !all_tags.is_empty() {
        fm.insert(key("tags"), serde_yaml_ng::Value::Sequence(all_tags.into_iter().map(serde_yaml_ng::Value::String).collect()));
    }
    for (k, v) in given {
        if !fm.contains_key(&k) {
            fm.insert(k, v);
        }
    }
    if let Some(m) = metadata {
        merge_metadata(&mut fm, m);
    }
    render(&fm, body)
}

/// A heading address: `Decisions`, `Auth/Decisions`, `Decisions[1]`.
pub fn find_section(lines: &[&str], spec: &str) -> Result<Heading> {
    let heads = note::headings(lines);
    let spec = spec.trim().trim_start_matches('#').trim();
    let (path, nth) = match spec.rsplit_once('[') {
        Some((p, n)) if n.ends_with(']') => (p, n.trim_end_matches(']').trim().parse::<usize>().context("section index must be a number")?),
        _ => (spec, 0),
    };
    let segs: Vec<String> = path.split('/').map(|s| crate::search::fold(s.trim().trim_start_matches('#').trim())).collect();
    let (last, parents) = segs.split_last().context("empty section")?;
    let found: Vec<&Heading> = heads
        .iter()
        .enumerate()
        .filter(|(_, h)| crate::search::fold(&h.text) == *last)
        .filter(|(i, h)| {
            // Walk the enclosing headings outward, matching parents innermost first.
            let mut level = h.level;
            let mut want = parents.iter().rev().peekable();
            for anc in heads[..*i].iter().rev() {
                if anc.level < level {
                    level = anc.level;
                    if want.peek().is_some_and(|w| **w == crate::search::fold(&anc.text)) {
                        want.next();
                    }
                }
            }
            want.peek().is_none()
        })
        .map(|(_, h)| h)
        .collect();
    match found.get(nth) {
        Some(h) => Ok((*h).clone()),
        None if found.is_empty() => {
            let names: Vec<&str> = heads.iter().map(|h| h.text.as_str()).collect();
            bail!("Section {spec:?} not found. Headings: {}", if names.is_empty() { "none".into() } else { names.join(", ") })
        }
        None => bail!("Section {spec:?} has only {} matches; [n] counts from 0", found.len()),
    }
}

/// End (exclusive line index) of the section that starts at `h`.
pub fn section_end(lines: &[&str], h: &Heading, with_subsections: bool) -> usize {
    note::headings(lines)
        .into_iter()
        .find(|o| o.idx > h.idx && (!with_subsections || o.level <= h.level))
        .map(|o| o.idx)
        .unwrap_or(lines.len())
}

fn join(lines: &[&str], trailing_newline: bool) -> String {
    let mut s = lines.join("\n");
    if trailing_newline {
        s.push('\n');
    }
    s
}

fn block(content: &str) -> Vec<&str> {
    content.trim_end_matches('\n').lines().collect()
}

pub enum Edit<'a> {
    Append(&'a str),
    Prepend(&'a str),
    FindReplace { find: &'a str, replace: &'a str, expected: usize },
    ReplaceSection { section: &'a str, content: &'a str, subsections: bool },
    InsertBefore { section: &'a str, content: &'a str },
    InsertAfter { section: &'a str, content: &'a str },
}

pub fn apply(text: &str, edit: Edit) -> Result<String> {
    let nl = text.ends_with('\n') || text.is_empty();
    match edit {
        Edit::Append(c) => {
            let mut out = text.trim_end_matches('\n').to_string();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(c.trim_end_matches('\n'));
            out.push('\n');
            Ok(out)
        }
        Edit::Prepend(c) => {
            let (yaml, body, _) = note::split_frontmatter(text);
            let head = &text[..text.len() - body.len()];
            let body = body.trim_start_matches('\n');
            let sep = if yaml.is_some() { "\n" } else { "" };
            Ok(format!("{head}{sep}{}\n\n{body}", c.trim_end_matches('\n')))
        }
        Edit::FindReplace { find, replace, expected } => {
            let count = text.matches(find).count();
            if count != expected {
                bail!("find_text occurs {count} times, expected {expected}; nothing was changed");
            }
            Ok(text.replace(find, replace))
        }
        Edit::ReplaceSection { section, content, subsections } => {
            let lines: Vec<&str> = text.lines().collect();
            let h = find_section(&lines, section)?;
            let end = section_end(&lines, &h, subsections);
            let new = block(content);
            // Content that repeats the heading replaces it instead of nesting a copy.
            let keeps_heading = new.first().is_some_and(|f| f.trim() == lines[h.idx].trim());
            let mut out: Vec<&str> = lines[..h.idx].to_vec();
            if !keeps_heading {
                out.push(lines[h.idx]);
                if new.first().is_some_and(|f| !f.trim().is_empty()) {
                    out.push("");
                }
            }
            out.extend(new);
            if end < lines.len() {
                out.push("");
                out.extend(&lines[end..]);
            }
            Ok(join(&out, nl))
        }
        Edit::InsertBefore { section, content } => {
            let lines: Vec<&str> = text.lines().collect();
            let h = find_section(&lines, section)?;
            let mut out: Vec<&str> = lines[..h.idx].to_vec();
            out.extend(block(content));
            out.push("");
            out.extend(&lines[h.idx..]);
            Ok(join(&out, nl))
        }
        Edit::InsertAfter { section, content } => {
            let lines: Vec<&str> = text.lines().collect();
            let h = find_section(&lines, section)?;
            let end = section_end(&lines, &h, true);
            let mut before: Vec<&str> = lines[..end].to_vec();
            while before.last().is_some_and(|l| l.trim().is_empty()) {
                before.pop();
            }
            before.push("");
            before.extend(block(content));
            if end < lines.len() {
                before.push("");
                before.extend(&lines[end..]);
            }
            Ok(join(&before, nl))
        }
    }
}

/// A read slice: `(text, next line to continue from)`, lines 1-based over the
/// whole file. `max_tokens` counts four characters a token and cuts at a
/// blank line or heading when one is near.
pub fn slice(
    text: &str,
    start: Option<usize>,
    end: Option<usize>,
    max_tokens: Option<usize>,
    first_line: usize,
) -> (String, Option<usize>) {
    let lines: Vec<&str> = text.lines().collect();
    let from = start.unwrap_or(first_line).max(1);
    let to = end.unwrap_or(lines.len()).min(lines.len());
    if from > to {
        return (String::new(), None);
    }
    let picked = &lines[from - 1..to];
    let Some(budget) = max_tokens.map(|t| t * 4) else {
        return (picked.join("\n"), None);
    };
    let mut used = 0;
    let mut cut = picked.len();
    let mut soft = None;
    for (i, l) in picked.iter().enumerate() {
        if used + l.len() + 1 > budget {
            cut = soft.filter(|s| *s > i / 2).unwrap_or(i.max(1));
            break;
        }
        used += l.len() + 1;
        if l.trim().is_empty() || l.starts_with('#') {
            soft = Some(if l.starts_with('#') { i } else { i + 1 });
        }
    }
    let next = (cut < picked.len()).then_some(from + cut);
    (picked[..cut].join("\n"), next)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\ntitle: T\n---\n\n# Auth\n\n## Decisions\na\n\n### Detail\nd\n\n# Other\n\n## Decisions\nb\n";

    #[test]
    fn sections_by_path_and_index() {
        let lines: Vec<&str> = DOC.lines().collect();
        assert_eq!(find_section(&lines, "Auth/Decisions").unwrap().idx, 6);
        assert_eq!(find_section(&lines, "Other/Decisions").unwrap().idx, 14);
        assert_eq!(find_section(&lines, "Decisions[1]").unwrap().idx, 14);
        assert!(find_section(&lines, "Nope").is_err());
    }

    #[test]
    fn edits() {
        let r = apply(DOC, Edit::ReplaceSection { section: "Auth/Decisions", content: "novo", subsections: true }).unwrap();
        assert!(r.contains("## Decisions\n\nnovo\n\n# Other"));
        let r = apply(DOC, Edit::ReplaceSection { section: "Auth/Decisions", content: "novo", subsections: false }).unwrap();
        assert!(r.contains("## Decisions\n\nnovo\n\n### Detail\nd"));
        let r = apply(DOC, Edit::InsertAfter { section: "Auth", content: "- x" }).unwrap();
        assert!(r.contains("d\n\n- x\n\n# Other"));
        let r = apply(DOC, Edit::InsertBefore { section: "Other", content: "antes" }).unwrap();
        assert!(r.contains("antes\n\n# Other"));
        let r = apply(DOC, Edit::Prepend("topo")).unwrap();
        assert!(r.starts_with("---\ntitle: T\n---\n\ntopo\n\n# Auth"));
        let r = apply("x", Edit::Append("y")).unwrap();
        assert_eq!(r, "x\n\ny\n");
        assert!(apply(DOC, Edit::FindReplace { find: "Decisions", replace: "D", expected: 1 }).is_err());
        assert_eq!(apply(DOC, Edit::FindReplace { find: "Decisions", replace: "D", expected: 2 }).unwrap().matches("## D\n").count(), 2);
    }

    #[test]
    fn new_note_frontmatter() {
        let t = compose_new("---\ntags: [a]\nstatus: x\n---\nbody\n", "T", "note", "main/t", &["b".into()], None).unwrap();
        assert_eq!(t, "---\ntitle: T\ntype: note\npermalink: main/t\ntags:\n- a\n- b\nstatus: x\n---\n\nbody\n");
    }

    #[test]
    fn slices() {
        let text = "l1\nl2\n\nl4\nl5\n";
        assert_eq!(slice(text, Some(2), Some(4), None, 1).0, "l2\n\nl4");
        let (s, next) = slice(text, None, None, Some(2), 1);
        assert_eq!((s.as_str(), next), ("l1\nl2\n", Some(4)));
    }
}
