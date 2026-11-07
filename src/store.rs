//! The index: one SQLite file derived entirely from the notes folder.
//! Deleting it loses nothing but the embedding cache, which is keyed by the
//! hash of each chunk's text and so survives renames and rewrites.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::note::{self, Note};

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
-- Statement journals and sorts stay in memory: the container's root (and /tmp) is read-only.
PRAGMA temp_store=MEMORY;
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS notes(
  id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, permalink TEXT NOT NULL, title TEXT NOT NULL,
  note_type TEXT NOT NULL, tags TEXT NOT NULL, frontmatter TEXT NOT NULL,
  checksum TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
  created INTEGER NOT NULL, updated INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS notes_permalink ON notes(permalink);
CREATE TABLE IF NOT EXISTS observations(
  id INTEGER PRIMARY KEY, note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  category TEXT, content TEXT NOT NULL, tags TEXT NOT NULL, context TEXT, line INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS observations_note ON observations(note_id);
CREATE TABLE IF NOT EXISTS relations(
  id INTEGER PRIMARY KEY, note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  rel_type TEXT NOT NULL, target TEXT NOT NULL, target_id INTEGER, context TEXT, line INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS relations_note ON relations(note_id);
CREATE INDEX IF NOT EXISTS relations_target ON relations(target_id);
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(title, body, permalink, tags, tokenize='unicode61 remove_diacritics 2');
CREATE VIRTUAL TABLE IF NOT EXISTS obs_fts USING fts5(content, category, tags, tokenize='unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS chunks(
  id INTEGER PRIMARY KEY, note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  obs_id INTEGER, text TEXT NOT NULL, hash TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS chunks_note ON chunks(note_id);
CREATE INDEX IF NOT EXISTS chunks_hash ON chunks(hash);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, tokenize='unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS vectors(hash TEXT NOT NULL, model TEXT NOT NULL, vec BLOB NOT NULL, PRIMARY KEY(hash, model));
";

/// Bumped whenever note parsing changes what it extracts.
const PARSER_VERSION: &str = "4";

/// Longest chunk embedded for a note section; longer sections are cut with overlap.
const CHUNK_CHARS: usize = 900;
const CHUNK_OVERLAP: usize = 120;

#[derive(Debug, Clone, Serialize)]
pub struct NoteRow {
    pub id: i64,
    pub path: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub tags: Vec<String>,
    pub frontmatter: Value,
    pub checksum: String,
    pub created: i64,
    pub updated: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObsRow {
    pub id: i64,
    pub note_id: i64,
    pub category: Option<String>,
    pub content: String,
    pub tags: Vec<String>,
    pub context: Option<String>,
    pub line: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelRow {
    pub id: i64,
    pub note_id: i64,
    pub rel_type: String,
    pub target: String,
    pub target_id: Option<i64>,
    pub context: Option<String>,
}

/// (chunk id, note id, observation id, unit vector)
pub type VectorRow = (i64, i64, Option<i64>, Vec<f32>);

pub struct Store {
    pub db: Mutex<Connection>,
    pub root: PathBuf,
    pub project: String,
}

/// The text an observation is indexed and embedded as.
pub fn observation_text(title: &str, category: Option<&str>, content: &str) -> String {
    let cat = category.map(|c| format!("{c}: ")).unwrap_or_default();
    format!("{title}\n\n{cat}{content}")
}

pub fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

const NOTE_COLS: &str = "id, path, permalink, title, note_type, tags, frontmatter, checksum, created, updated";

fn note_row(r: &rusqlite::Row) -> rusqlite::Result<NoteRow> {
    Ok(NoteRow {
        id: r.get(0)?,
        path: r.get(1)?,
        permalink: r.get(2)?,
        title: r.get(3)?,
        note_type: r.get(4)?,
        tags: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
        frontmatter: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or(Value::Null),
        checksum: r.get(7)?,
        created: r.get(8)?,
        updated: r.get(9)?,
    })
}

const OBS_COLS: &str = "id, note_id, category, content, tags, context, line";

fn obs_row(r: &rusqlite::Row) -> rusqlite::Result<ObsRow> {
    Ok(ObsRow {
        id: r.get(0)?,
        note_id: r.get(1)?,
        category: r.get(2)?,
        content: r.get(3)?,
        tags: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        context: r.get(5)?,
        line: r.get(6)?,
    })
}

const REL_COLS: &str = "id, note_id, rel_type, target, target_id, context";

fn rel_row(r: &rusqlite::Row) -> rusqlite::Result<RelRow> {
    Ok(RelRow { id: r.get(0)?, note_id: r.get(1)?, rel_type: r.get(2)?, target: r.get(3)?, target_id: r.get(4)?, context: r.get(5)? })
}

/// Splits a note into embedding chunks: each heading section (title first,
/// for context), cut at `CHUNK_CHARS`; then one chunk per observation and one per relation.
pub fn chunks(n: &Note) -> Vec<(Option<usize>, String)> {
    let mut out = Vec::new();
    let mut section = String::new();
    let flush = |section: &mut String, out: &mut Vec<(Option<usize>, String)>| {
        let text = section.trim();
        if !text.is_empty() {
            for piece in split_chars(text) {
                out.push((None, format!("{}\n\n{}", n.title, piece)));
            }
        }
        section.clear();
    };
    for line in n.body.lines() {
        if line.starts_with('#') && line.trim_start_matches('#').starts_with(' ') {
            flush(&mut section, &mut out);
        }
        section.push_str(line);
        section.push('\n');
    }
    flush(&mut section, &mut out);
    for (i, o) in n.observations.iter().enumerate() {
        out.push((Some(i), observation_text(&n.title, o.category.as_deref(), &o.content)));
    }
    for r in &n.relations {
        let context = r.context.as_deref().map(|c| format!("\n\n{c}")).unwrap_or_default();
        out.push((None, format!("{} -> {}\n\n{}{context}", n.title, r.target, r.rel_type)));
    }
    out
}

fn split_chars(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= CHUNK_CHARS {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut end = (start + CHUNK_CHARS).min(chars.len());
        if end < chars.len()
            && let Some(nl) = (start + CHUNK_CHARS / 2..end).rev().find(|&i| chars[i] == '\n')
        {
            end = nl;
        }
        out.push(chars[start..end].iter().collect::<String>().trim().to_string());
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP).max(start + 1);
    }
    out.retain(|c| !c.is_empty());
    out
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// Every `.md` file under `root`, as project-relative paths with `/`.
pub fn walk(root: &Path) -> Vec<String> {
    walk_where(root, |name| name.ends_with(".md"))
}

/// Every file under `root`, hidden ones aside.
pub fn walk_all(root: &Path) -> Vec<String> {
    walk_where(root, |_| true)
}

fn walk_where(root: &Path, keep: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() && keep(&name) => {
                    if let Ok(rel) = p.strip_prefix(root) {
                        out.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

impl Store {
    pub fn open(db_path: &Path, root: PathBuf, project: String) -> Result<Self> {
        let conn = Connection::open(db_path).with_context(|| format!("opening {}", db_path.display()))?;
        conn.execute_batch(SCHEMA)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        // Notes parsed by other rules are re-read on the next sync; vectors stay cached.
        let parser: Option<String> = conn.query_row("SELECT value FROM meta WHERE key='parser'", [], |r| r.get(0)).optional()?;
        if parser.as_deref() != Some(PARSER_VERSION) {
            conn.execute("UPDATE notes SET checksum='', size=-1", [])?;
            conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('parser', ?1)", [PARSER_VERSION])?;
        }
        Ok(Self { db: Mutex::new(conn), root, project })
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Brings the index in line with the folder. Files are re-read when their
    /// size or mtime moved, and re-indexed only when their bytes changed.
    /// Returns how many notes were added, changed or removed.
    pub fn sync(&self) -> Result<usize> {
        let files = walk(&self.root);
        let known: HashMap<String, (i64, i64, String)> = {
            let c = self.conn();
            let mut st = c.prepare("SELECT path, size, mtime, checksum FROM notes")?;
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?.collect::<Result<_, _>>()?
        };
        let mut changed = 0;
        for rel in &files {
            let Ok(meta) = std::fs::metadata(self.abs(rel)) else { continue };
            let (size, mtime) = (meta.len() as i64, mtime_of(&meta));
            if let Some((s, m, _)) = known.get(rel)
                && *s == size
                && *m == mtime
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(self.abs(rel)) else { continue };
            let sum = checksum(&bytes);
            if known.get(rel).is_some_and(|(_, _, c)| *c == sum) {
                self.conn().execute("UPDATE notes SET size=?1, mtime=?2 WHERE path=?3", params![size, mtime, rel])?;
                continue;
            }
            self.index_bytes(rel, &bytes, size, mtime)?;
            changed += 1;
        }
        let present: HashSet<&String> = files.iter().collect();
        for rel in known.keys().filter(|k| !present.contains(k)) {
            self.remove(rel)?;
            changed += 1;
        }
        if changed > 0 {
            self.resolve_relations()?;
        }
        Ok(changed)
    }

    /// Re-indexes one file now, after a tool wrote it.
    pub fn index_file(&self, rel: &str) -> Result<()> {
        let path = self.abs(rel);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {rel}"))?;
        let meta = std::fs::metadata(&path)?;
        self.index_bytes(rel, &bytes, meta.len() as i64, mtime_of(&meta))?;
        self.resolve_relations()
    }

    fn index_bytes(&self, rel: &str, bytes: &[u8], size: i64, mtime: i64) -> Result<()> {
        let text = String::from_utf8_lossy(bytes);
        let n = note::parse(&self.project, rel, &text);
        let sum = checksum(bytes);
        let fm = note::fm_json(&n.frontmatter).to_string();
        let tags = serde_json::to_string(&n.tags)?;
        let now = now();
        let mut c = self.conn();
        let tx = c.transaction()?;
        let existing: Option<(i64, i64)> =
            tx.query_row("SELECT id, created FROM notes WHERE path=?1", [rel], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let id = match existing {
            Some((id, _)) => {
                tx.execute(
                    "UPDATE notes SET permalink=?1, title=?2, note_type=?3, tags=?4, frontmatter=?5, checksum=?6, size=?7, mtime=?8, updated=?9 WHERE id=?10",
                    params![n.permalink, n.title, n.note_type, tags, fm, sum, size, mtime, mtime / 1_000_000_000, id],
                )?;
                tx.execute("DELETE FROM notes_fts WHERE rowid=?1", [id])?;
                tx.execute("DELETE FROM obs_fts WHERE rowid IN (SELECT id FROM observations WHERE note_id=?1)", [id])?;
                tx.execute("DELETE FROM observations WHERE note_id=?1", [id])?;
                tx.execute("DELETE FROM relations WHERE note_id=?1", [id])?;
                tx.execute("DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE note_id=?1)", [id])?;
                tx.execute("DELETE FROM chunks WHERE note_id=?1", [id])?;
                id
            }
            None => {
                let created = if mtime > 0 { mtime / 1_000_000_000 } else { now };
                tx.execute(
                    "INSERT INTO notes(path, permalink, title, note_type, tags, frontmatter, checksum, size, mtime, created, updated) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
                    params![rel, n.permalink, n.title, n.note_type, tags, fm, sum, size, mtime, created],
                )?;
                tx.last_insert_rowid()
            }
        };
        tx.execute(
            "INSERT INTO notes_fts(rowid, title, body, permalink, tags) VALUES(?1,?2,?3,?4,?5)",
            params![id, n.title, n.body, n.permalink, n.tags.join(" ")],
        )?;
        let mut obs_ids = Vec::with_capacity(n.observations.len());
        for o in &n.observations {
            let otags = serde_json::to_string(&o.tags)?;
            tx.execute(
                "INSERT INTO observations(note_id, category, content, tags, context, line) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, o.category, o.content, otags, o.context, o.line as i64],
            )?;
            let oid = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO obs_fts(rowid, content, category, tags) VALUES(?1,?2,?3,?4)",
                params![oid, o.content, o.category.clone().unwrap_or_default(), o.tags.join(" ")],
            )?;
            obs_ids.push(oid);
        }
        for r in &n.relations {
            tx.execute(
                "INSERT INTO relations(note_id, rel_type, target, context, line) VALUES(?1,?2,?3,?4,?5)",
                params![id, r.rel_type, r.target, r.context, r.line as i64],
            )?;
        }
        for (obs, text) in chunks(&n) {
            tx.execute(
                "INSERT INTO chunks(note_id, obs_id, text, hash) VALUES(?1,?2,?3,?4)",
                params![id, obs.map(|i| obs_ids[i]), text, checksum(text.as_bytes())],
            )?;
            tx.execute("INSERT INTO chunks_fts(rowid, text) VALUES(?1, ?2)", params![tx.last_insert_rowid(), text])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove(&self, rel: &str) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        if let Some(id) = tx.query_row("SELECT id FROM notes WHERE path=?1", [rel], |r| r.get::<_, i64>(0)).optional()? {
            tx.execute("DELETE FROM notes_fts WHERE rowid=?1", [id])?;
            tx.execute("DELETE FROM obs_fts WHERE rowid IN (SELECT id FROM observations WHERE note_id=?1)", [id])?;
            tx.execute("DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE note_id=?1)", [id])?;
            tx.execute("UPDATE relations SET target_id=NULL WHERE target_id=?1", [id])?;
            tx.execute("DELETE FROM notes WHERE id=?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Points every relation at its note: by exact title, permalink or path,
    /// then by slug, which forgives `:` written as `-` and the like.
    pub fn resolve_relations(&self) -> Result<()> {
        let mut c = self.conn();
        let notes: Vec<(i64, String, String, String)> = {
            let mut st = c.prepare("SELECT id, title, permalink, path FROM notes")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?
        };
        let mut exact: HashMap<String, i64> = HashMap::new();
        let mut slug: HashMap<String, i64> = HashMap::new();
        let prefix = format!("{}/", self.project);
        for (id, title, permalink, path) in &notes {
            let stem = path.strip_suffix(".md").unwrap_or(path);
            for k in [title.to_lowercase(), permalink.to_lowercase(), stem.to_lowercase(), path.to_lowercase()] {
                exact.entry(k).or_insert(*id);
            }
            slug.entry(note::slugify(title)).or_insert(*id);
            slug.entry(note::slugify(stem.rsplit('/').next().unwrap_or(stem))).or_insert(*id);
            slug.entry(permalink.strip_prefix(&prefix).unwrap_or(permalink).to_string()).or_insert(*id);
        }
        let rels: Vec<(i64, String, Option<i64>)> = {
            let mut st = c.prepare("SELECT id, target, target_id FROM relations")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?
        };
        let tx = c.transaction()?;
        for (id, target, current) in rels {
            let t = target.trim_start_matches("memory://");
            let t = t.strip_prefix(&prefix).unwrap_or(t);
            let found =
                exact.get(&target.to_lowercase()).or_else(|| exact.get(&t.to_lowercase())).or_else(|| slug.get(&note::slugify(t))).copied();
            if found != current {
                tx.execute("UPDATE relations SET target_id=?1 WHERE id=?2", params![found, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A note by path, permalink, memory:// URL, `<project>/path` or title.
    pub fn resolve(&self, identifier: &str) -> Result<Option<NoteRow>> {
        let id = identifier.trim().trim_start_matches("memory://");
        let prefix = format!("{}/", self.project);
        let bare = id.strip_prefix(&prefix).unwrap_or(id).trim_start_matches('/');
        let with_md = if bare.ends_with(".md") { bare.to_string() } else { format!("{bare}.md") };
        let c = self.conn();
        let one = |sql: &str, v: &str| -> Result<Option<NoteRow>> {
            Ok(c.query_row(&format!("SELECT {NOTE_COLS} FROM notes WHERE {sql} LIMIT 1"), [v], note_row).optional()?)
        };
        for (sql, v) in [
            ("path = ?1", bare.to_string()),
            ("path = ?1", with_md.clone()),
            ("permalink = ?1", format!("{prefix}{bare}")),
            ("permalink = ?1", bare.to_string()),
            ("lower(title) = lower(?1)", id.to_string()),
            ("lower(path) = lower(?1)", with_md),
        ] {
            if let Some(n) = one(sql, &v)? {
                return Ok(Some(n));
            }
        }
        let slug = note::slugify(bare);
        drop(c);
        Ok(self.all_notes()?.into_iter().find(|n| {
            note::slugify(&n.title) == slug
                || n.permalink.strip_prefix(&prefix).unwrap_or(&n.permalink) == slug
                || note::slugify(n.path.trim_end_matches(".md")) == slug
        }))
    }

    pub fn require(&self, identifier: &str) -> Result<NoteRow> {
        match self.resolve(identifier)? {
            Some(n) => Ok(n),
            None => bail!("Note not found: {identifier}. Find notes with mem_search or mem_list."),
        }
    }

    pub fn note_by_id(&self, id: i64) -> Result<Option<NoteRow>> {
        Ok(self.conn().query_row(&format!("SELECT {NOTE_COLS} FROM notes WHERE id=?1"), [id], note_row).optional()?)
    }

    pub fn all_notes(&self) -> Result<Vec<NoteRow>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {NOTE_COLS} FROM notes ORDER BY path"))?;
        Ok(st.query_map([], note_row)?.collect::<Result<_, _>>()?)
    }

    pub fn observations(&self, note_id: i64) -> Result<Vec<ObsRow>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {OBS_COLS} FROM observations WHERE note_id=?1 ORDER BY line"))?;
        Ok(st.query_map([note_id], obs_row)?.collect::<Result<_, _>>()?)
    }

    pub fn observation(&self, id: i64) -> Result<Option<ObsRow>> {
        Ok(self.conn().query_row(&format!("SELECT {OBS_COLS} FROM observations WHERE id=?1"), [id], obs_row).optional()?)
    }

    pub fn outgoing(&self, note_id: i64) -> Result<Vec<RelRow>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {REL_COLS} FROM relations WHERE note_id=?1 ORDER BY line"))?;
        Ok(st.query_map([note_id], rel_row)?.collect::<Result<_, _>>()?)
    }

    pub fn incoming(&self, note_id: i64) -> Result<Vec<RelRow>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {REL_COLS} FROM relations WHERE target_id=?1 ORDER BY note_id"))?;
        Ok(st.query_map([note_id], rel_row)?.collect::<Result<_, _>>()?)
    }

    pub fn all_relations(&self) -> Result<Vec<RelRow>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {REL_COLS} FROM relations"))?;
        Ok(st.query_map([], rel_row)?.collect::<Result<_, _>>()?)
    }

    pub fn observations_since(&self, since: i64) -> Result<Vec<ObsRow>> {
        let c = self.conn();
        let mut st = c.prepare(&format!(
            "SELECT {} FROM observations o JOIN notes n ON n.id=o.note_id WHERE n.updated >= ?1 ORDER BY n.updated DESC, o.line",
            OBS_COLS.split(", ").map(|c| format!("o.{c}")).collect::<Vec<_>>().join(", ")
        ))?;
        Ok(st.query_map([since], obs_row)?.collect::<Result<_, _>>()?)
    }

    /// Full-text hits over notes, best first: (note id, bm25 rank, excerpt).
    pub fn fts_notes(&self, expr: &str, limit: usize) -> Result<Vec<(i64, f64, String)>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT rowid, bm25(notes_fts, 8.0, 1.0, 4.0, 3.0), snippet(notes_fts, 1, '', '', '…', 24) FROM notes_fts WHERE notes_fts MATCH ?1 ORDER BY 2 LIMIT ?2",
        )?;
        let rows = st.query_map(params![expr, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Full-text hits over chunks, best first: (note id, chunk id, bm25 rank, excerpt).
    pub fn fts_chunks(&self, expr: &str, limit: usize) -> Result<Vec<(i64, i64, f64, String)>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT c.note_id, c.id, bm25(chunks_fts), snippet(chunks_fts, 0, '', '', '…', 24) FROM chunks_fts JOIN chunks c ON c.id=chunks_fts.rowid WHERE chunks_fts MATCH ?1 ORDER BY 3 LIMIT ?2",
        )?;
        let rows = st.query_map(params![expr, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Observations in any of these categories, compared without case.
    pub fn observations_in(&self, categories: &[String]) -> Result<HashSet<i64>> {
        if categories.is_empty() {
            return Ok(HashSet::new());
        }
        let c = self.conn();
        let mut st = c.prepare("SELECT id, category FROM observations WHERE category IS NOT NULL")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let want: HashSet<String> = categories.iter().map(|c| c.to_lowercase()).collect();
        let mut out = HashSet::new();
        for row in rows {
            let (id, cat) = row?;
            if want.contains(&cat.to_lowercase()) {
                out.insert(id);
            }
        }
        Ok(out)
    }

    pub fn fts_observations(&self, expr: &str, limit: usize) -> Result<Vec<(i64, f64)>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT rowid, bm25(obs_fts) FROM obs_fts WHERE obs_fts MATCH ?1 ORDER BY 2 LIMIT ?2")?;
        let rows = st.query_map(params![expr, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn chunk_text(&self, id: i64) -> Result<Option<String>> {
        Ok(self.conn().query_row("SELECT text FROM chunks WHERE id=?1", [id], |r| r.get(0)).optional()?)
    }

    /// Chunks whose text has no vector for `model` yet, one per distinct text.
    pub fn unembedded(&self, model: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT hash, min(text) FROM chunks c WHERE NOT EXISTS (SELECT 1 FROM vectors v WHERE v.hash=c.hash AND v.model=?1) GROUP BY hash LIMIT ?2",
        )?;
        let rows = st.query_map(params![model, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn put_vectors(&self, model: &str, items: &[(String, Vec<f32>)]) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        for (hash, v) in items {
            let blob: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            tx.execute("INSERT OR REPLACE INTO vectors(hash, model, vec) VALUES(?1,?2,?3)", params![hash, model, blob])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every embedded chunk: (chunk id, note id, observation id, vector).
    pub fn vectors(&self, model: &str) -> Result<Vec<VectorRow>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT c.id, c.note_id, c.obs_id, v.vec FROM chunks c JOIN vectors v ON v.hash=c.hash AND v.model=?1")?;
        let rows = st.query_map([model], |r| {
            let blob: Vec<u8> = r.get(3)?;
            let v = blob.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, v))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn embedding_counts(&self, model: &str) -> Result<(i64, i64)> {
        let c = self.conn();
        let total: i64 = c.query_row("SELECT count(DISTINCT hash) FROM chunks", [], |r| r.get(0))?;
        let done: i64 =
            c.query_row("SELECT count(DISTINCT c.hash) FROM chunks c JOIN vectors v ON v.hash=c.hash AND v.model=?1", [model], |r| {
                r.get(0)
            })?;
        Ok((total, done))
    }

    /// Drops cached vectors no chunk uses any more.
    /// Stored vectors for these text hashes, by hash.
    pub fn vectors_for(&self, model: &str, hashes: &[String]) -> Result<HashMap<String, Vec<f32>>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT vec FROM vectors WHERE hash=?1 AND model=?2")?;
        let mut out = HashMap::new();
        for h in hashes {
            if let Some(blob) = st.query_row(params![h, model], |r| r.get::<_, Vec<u8>>(0)).optional()? {
                out.insert(h.clone(), blob.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect());
            }
        }
        Ok(out)
    }

    pub fn prune_vectors(&self) -> Result<usize> {
        // Vectors under "<model>:bare" are a cache of observation texts without the title (duplicate checks).
        Ok(self.conn().execute("DELETE FROM vectors WHERE hash NOT IN (SELECT hash FROM chunks) AND model NOT LIKE '%:bare'", [])?)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn store_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("notes");
        for (p, t) in files {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, t).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(&dir.path().join("index.db"), root, "main".into()).unwrap();
        store.sync().unwrap();
        (dir, store)
    }

    #[test]
    fn sync_indexes_resolves_and_forgets() {
        let (_d, s) = store_with(&[
            ("a/Wi-Fi- causa.md", "---\ntitle: 'Wi-Fi: causa'\n---\n- [fato] x #t\n"),
            ("b/Outra.md", "- relates_to [[Wi-Fi: causa]]\n- part_of [[wi-fi-causa]]\n- x [[Nada]]\n"),
        ]);
        let b = s.require("b/Outra").unwrap();
        let a = s.require("Wi-Fi: causa").unwrap();
        assert_eq!(a.permalink, "main/a/wi-fi-causa");
        assert!(s.resolve("memory://main/a/wi-fi-causa").unwrap().is_some());
        let out = s.outgoing(b.id).unwrap();
        assert_eq!(out.iter().map(|r| r.target_id).collect::<Vec<_>>(), [Some(a.id), Some(a.id), None]);
        assert_eq!(s.incoming(a.id).unwrap().len(), 2);
        assert_eq!(s.observations(a.id).unwrap()[0].category.as_deref(), Some("fato"));
        assert_eq!(s.sync().unwrap(), 0);
        std::fs::remove_file(s.abs("a/Wi-Fi- causa.md")).unwrap();
        assert_eq!(s.sync().unwrap(), 1);
        assert_eq!(s.outgoing(b.id).unwrap()[0].target_id, None);
    }

    #[test]
    fn rewrite_replaces_observations() {
        let (_d, s) = store_with(&[("n.md", "- [decisao] x\n")]);
        std::fs::write(s.abs("n.md"), "- [decisão] x\n").unwrap();
        s.index_file("n.md").unwrap();
        let n = s.require("n").unwrap();
        assert_eq!(s.observations(n.id).unwrap()[0].category.as_deref(), Some("decisão"));
        assert_eq!(s.fts_observations("decisao", 5).unwrap().len(), 1);
        // Chunk text follows the rewrite and goes with the note.
        // One section chunk and one observation chunk.
        assert_eq!(s.fts_chunks("decisao", 5).unwrap().len(), 2);
        std::fs::write(s.abs("n.md"), "- [fato] y\n").unwrap();
        s.index_file("n.md").unwrap();
        assert!(s.fts_chunks("decisao", 5).unwrap().is_empty());
        s.remove("n.md").unwrap();
        assert!(s.fts_chunks("fato", 5).unwrap().is_empty());
    }

    #[test]
    fn long_sections_split_with_overlap() {
        let text = "linha de texto\n".repeat(200);
        let parts = split_chars(&text);
        assert!(parts.len() > 2 && parts.iter().all(|p| p.chars().count() <= CHUNK_CHARS));
    }
}
