//! The MCP tools: the `mem_*` surface clients call.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;

use base64::Engine;
use rmcp::handler::server::{router::tool::ToolRouter, tool::IntoCallToolResult, wrapper::Parameters};
use rmcp::model::*;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::embed::{self, Embedder, Vectors};
use crate::note;
use crate::search::{self, Hit, Mode, Scope};
use crate::store::{self, NoteRow, ObsRow, RelRow, Store, checksum, now};
use crate::vault::{self, Edit};

pub struct Ctx {
    pub store: Arc<Store>,
    pub embedder: Option<Arc<Embedder>>,
    pub vectors: Arc<Vectors>,
    /// Hides every tool that writes.
    pub read_only: bool,
    /// Accepted in `project` besides the project name.
    pub project_id: Option<String>,
    /// Observation categories that rank after current facts, such as "superseded".
    pub demoted: Vec<String>,
}

#[derive(Clone)]
pub struct Server {
    ctx: Arc<Ctx>,
    tool_router: ToolRouter<Server>,
}

/// A failed call: `Tool` reaches the caller as an `isError` result with the
/// message; `Args` stays a JSON-RPC invalid_params error.
#[derive(Debug)]
enum Fail {
    Tool(String),
    Args(ErrorData),
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        Fail::Tool(format!("{e:#}"))
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Tool(e.to_string())
    }
}

impl IntoCallToolResult for Fail {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        match self {
            Fail::Tool(message) => Ok(CallToolResult::error(vec![ContentBlock::text(message)]).into()),
            Fail::Args(e) => Err(e),
        }
    }
}

fn fail<T>(message: impl Into<String>) -> Result<T, Fail> {
    Err(Fail::Tool(message.into()))
}

fn bad_args<T>(message: impl Into<String>) -> Result<T, Fail> {
    Err(Fail::Args(ErrorData::invalid_params(message.into(), None)))
}

type R = Result<CallToolResult, Fail>;

fn text(t: impl Into<String>) -> R {
    Ok(CallToolResult::success(vec![ContentBlock::text(t)]))
}

fn json_out(v: Value) -> R {
    text(serde_json::to_string_pretty(&v).unwrap_or_else(|_| "null".into()))
}

#[derive(Deserialize, JsonSchema, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Text,
    Json,
}

fn d1() -> usize {
    1
}
fn d10() -> usize {
    10
}
fn dtrue() -> bool {
    true
}
fn droot() -> String {
    "/".into()
}
fn d7d() -> String {
    "7d".into()
}

fn date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

fn paginate<T>(items: Vec<T>, page: usize, size: usize) -> (Vec<T>, usize) {
    let total = items.len();
    let size = size.clamp(1, 100);
    (items.into_iter().skip((page.max(1) - 1) * size).take(size).collect(), total)
}

fn pages_line(page: usize, size: usize, total: usize) -> String {
    let pages = total.div_ceil(size.clamp(1, 100)).max(1);
    format!("page {page} of {pages}, {total} total")
}

fn note_json(n: &NoteRow) -> Value {
    json!({
        "title": n.title, "path": n.path, "permalink": n.permalink, "type": n.note_type,
        "tags": n.tags, "checksum": n.checksum, "updated_at": date(n.updated), "created_at": date(n.created),
    })
}

fn obs_line(o: &ObsRow) -> String {
    let cat = o.category.as_deref().map(|c| format!("[{c}] ")).unwrap_or_default();
    let ctx = o.context.as_deref().map(|c| format!(" ({c})")).unwrap_or_default();
    format!("- {cat}{}{ctx}", o.content)
}

fn obs_json(o: &ObsRow) -> Value {
    json!({ "category": o.category, "content": o.content, "tags": o.tags, "context": o.context, "line": o.line })
}

fn cut(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>().trim_end()) }
}

/// Paths from callers stay inside the notes folder.
/// Cosine similarity, on the observations' own text, from which two say the same thing. Calibrated
/// with bge-m3 on a real knowledge base: known repeats scored 0.86-0.97, related facts up to 0.84.
const SAME_FACT: f32 = 0.85;
/// The shortlist: a bare observation against the stored vectors, which include the note title
/// (known repeats scored 0.77-0.84 this way).
const SHORTLIST: f32 = 0.72;
/// Candidates re-embedded per added observation, and in all: each costs the embedding server a text.
const SHORTLIST_SIZE: usize = 3;
const SHORTLIST_TOTAL: usize = 8;
/// Added observations compared per write, and repeats reported.
const MAX_COMPARED: usize = 5;
/// How long a write may wait for the repeat check (on a CPU, one embedded text takes ~0.7 s).
const REPEAT_CHECK_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_SIMILAR: usize = 5;

/// Short items ("Next steps:") look alike without saying the same thing.
fn worth_comparing(s: &str) -> bool {
    s.chars().count() >= 30
}

/// Readings that differ only in their numbers (weights, dates, versions) are not repeats.
fn same_numbers(a: &str, b: &str) -> bool {
    let nums = |s: &str| -> HashSet<String> {
        s.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
            .map(|t| t.trim_matches(|c| c == '.' || c == ','))
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (x, y) = (nums(a), nums(b));
    x.is_empty() || y.is_empty() || x == y
}

fn clean_rel(project: &str, p: &str) -> Result<String, Fail> {
    let p = p.trim().trim_start_matches("memory://");
    let p = p.strip_prefix(&format!("{project}/")).unwrap_or(p).trim_matches('/');
    if p.split('/').any(|seg| seg == ".." || seg.starts_with('.')) || p.contains('\\') || p.contains('\0') {
        return bad_args(format!("path {p:?} must stay inside the project and not name hidden files"));
    }
    Ok(p.to_string())
}

/// `clean_rel`, and no folder or file on the way is a symlink, which could lead outside.
fn inside(store: &Store, p: &str) -> Result<String, Fail> {
    let rel = clean_rel(&store.project, p)?;
    let mut at = store.abs("");
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        at.push(seg);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => return bad_args(format!("path {p:?} goes through a symlink")),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(rel)
}

fn with_md(p: &str) -> String {
    if p.ends_with(".md") { p.to_string() } else { format!("{p}.md") }
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// What to search for. Omit to search by filters alone.
    query: Option<String>,
    /// Search mode: hybrid (default when semantic search is available), text, semantic, title or permalink.
    mode: Option<String>,
    /// Frontmatter filters as {field: condition}: a value for equality, null for absent-or-null, a list for contains-all, or an operator object with $gt, $gte, $lt, $lte, $in, $between, $contains or $exists. Nested fields use dot notation.
    filters: Option<Map<String, Value>>,
    /// Frontmatter `type` values, e.g. ['person', 'meeting'].
    note_types: Option<Vec<String>>,
    /// What to return: note, observation, relation, or a mix. Default: note.
    items: Option<Vec<String>>,
    /// Observation categories to match exactly, e.g. ['decision'].
    categories: Option<Vec<String>>,
    /// How far back to look: '7d', '24h', '2 days ago', 'last week', or a date.
    since: Option<String>,
    /// Return each hit's matching lines with this many lines of context (0-10), instead of an excerpt.
    lines: Option<usize>,
    /// Matching lines shown per note with `lines`.
    #[serde(default = "d10")]
    max_matches: usize,
    /// Search only this note; returns its matching lines.
    identifier: Option<String>,
    /// Search only this folder and its subfolders.
    path: Option<String>,
    /// Omit excerpts; keep identifiers and scores.
    #[serde(default)]
    compact: bool,
    /// Override the semantic similarity floor (0-1).
    min_similarity: Option<f32>,
    /// Page number, from 1.
    #[serde(default = "d1")]
    page: usize,
    /// Results per page (1-100).
    #[serde(default = "d10")]
    page_size: usize,
    /// Project name. Optional: this server holds one project, used when this is omitted.
    project: Option<String>,
    /// Search these projects together.
    projects: Option<Vec<String>>,
    /// Search every project.
    #[serde(default)]
    #[expect(dead_code, reason = "accepted for compatibility: there is one project")]
    all_projects: bool,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
    /// Not supported here: temporal qualifiers are not indexed.
    time_kind: Option<String>,
    /// Not supported here.
    valid_at: Option<String>,
    /// Not supported here.
    valid_overlaps: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// The note or file: a path ('notes/plan.md'), permalink, memory:// URL, exact title, or '<project>/path'.
    identifier: String,
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// First line to return, counting the whole file (frontmatter included) from 1.
    start_line: Option<usize>,
    /// Last line to return, inclusive.
    end_line: Option<usize>,
    /// A heading to read: "Decisions", "Auth/Decisions", or "Decisions[1]" for the second one.
    section: Option<String>,
    /// Approximate token budget; longer content is cut at a section or paragraph boundary.
    max_tokens: Option<usize>,
    /// False returns only title, paths, checksum and updated_at.
    #[serde(default = "dtrue")]
    include_content: bool,
    /// Include the YAML frontmatter in a whole-note read.
    #[serde(default = "dtrue")]
    include_frontmatter: bool,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// The note's Markdown, saved as given; it may start with frontmatter.
    content: String,
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// File path for the note, e.g. 'notes/plan.md'; '.md' is added when missing. Use instead of title + directory.
    identifier: Option<String>,
    /// Note title. With `directory`, it also names the file.
    title: Option<String>,
    /// Folder for a note addressed by title; '/' is the root.
    directory: Option<String>,
    /// Frontmatter `type:`. A `type:` in the content wins.
    note_type: Option<String>,
    /// Tags, as a list or 'a,b,c'.
    tags: Option<Value>,
    /// Extra frontmatter fields, merged into the note's frontmatter.
    metadata: Option<Map<String, Value>>,
    /// Replace a note that already exists.
    overwrite: Option<bool>,
    /// Replace the note only while it is still this revision (the checksum from mem_read). Implies overwrite.
    expected_checksum: Option<String>,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EditCommand {
    Append,
    Prepend,
    FindReplace,
    ReplaceSection,
    InsertBeforeSection,
    InsertAfterSection,
    SetMetadata,
}

#[derive(Deserialize, JsonSchema)]
pub struct EditArgs {
    /// The note: a path, permalink, memory:// URL or exact title.
    identifier: String,
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// The edit: append, prepend, find_replace, replace_section, insert_before_section, insert_after_section or set_metadata.
    command: EditCommand,
    /// The text to add or the replacement. Not used by set_metadata.
    content: Option<String>,
    /// For find_replace: the exact, case-sensitive text to replace.
    find_text: Option<String>,
    /// For find_replace: how many times find_text must occur.
    #[serde(default = "d1")]
    expected_replacements: usize,
    /// The heading for section commands: "Decisions", "Auth/Decisions", or "Decisions[1]".
    section: Option<String>,
    /// For replace_section: true replaces the subsections too; false stops at the first subsection heading.
    #[serde(default = "dtrue")]
    replace_subsections: bool,
    /// Frontmatter fields to set; other fields are kept.
    metadata: Option<Map<String, Value>>,
    /// Edit only while the note is still this revision (from mem_read).
    expected_checksum: Option<String>,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveArgs {
    /// New path, e.g. 'archive/plan.md'. A trailing '/' moves into that folder under the current name.
    destination: String,
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// The note to move.
    identifier: Option<String>,
    /// The directory to move, with everything in it.
    path: Option<String>,
    /// Move the note only while it is still this revision (from mem_read).
    expected_checksum: Option<String>,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeleteArgs {
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// The note to delete.
    identifier: Option<String>,
    /// The directory to delete, with everything in it.
    path: Option<String>,
    /// Required true to delete a directory.
    #[serde(default)]
    recursive: bool,
    /// Delete the note only while it is still this revision (from mem_read).
    expected_checksum: Option<String>,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    TitleAsc,
    TitleDesc,
    UpdatedAsc,
    UpdatedDesc,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListArgs {
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// A directory in the project; '/' is the root.
    #[serde(default = "droot")]
    path: String,
    /// File-name glob, e.g. '*.md' or '*meeting*'.
    glob: Option<String>,
    /// File order. Folders always come first.
    sort: Option<Sort>,
    /// Page number, from 1.
    #[serde(default = "d1")]
    page: usize,
    /// Results per page.
    #[serde(default = "d10")]
    page_size: usize,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
pub struct FindArgs {
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// File-name glob, e.g. '*meeting*'. Not with `filters`.
    name: Option<String>,
    /// Frontmatter filters, as in mem_search.
    filters: Option<Map<String, Value>>,
    /// Frontmatter fields to return per note (dot paths allowed). Needs `filters`.
    fields: Option<Vec<String>>,
    /// A directory to search in; '/' is the root.
    #[serde(default = "droot")]
    path: String,
    /// Folder levels to search (1-10).
    #[serde(default = "d10")]
    depth: usize,
    /// Page number, from 1.
    #[serde(default = "d1")]
    page: usize,
    /// Results per page.
    #[serde(default = "d10")]
    page_size: usize,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
pub struct ContextArgs {
    /// Where to start: a note path, permalink, memory:// URL, or a pattern such as 'specs/*'.
    identifier: String,
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// Relation hops to follow (1-5).
    #[serde(default = "d1")]
    depth: usize,
    /// Only related notes changed this recently: '7d', '24h', 'last week', or a date. Default: no limit.
    since: Option<String>,
    /// Most related results to return (0-100).
    #[serde(default = "d10")]
    max_related: usize,
    /// Omit note bodies and observations; keep the graph shape.
    #[serde(default)]
    compact: bool,
    /// Page number, from 1.
    #[serde(default = "d1")]
    page: usize,
    /// Primary results per page (1-50).
    #[serde(default = "d10")]
    page_size: usize,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCommand {
    List,
    Create,
    Recent,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectArgs {
    /// list shows the project; recent shows what changed lately; create is refused (one project per server).
    command: ProjectCommand,
    /// The project name. Optional: this server holds one project, used when this is omitted.
    project: Option<String>,
    /// For recent: notes, observations, relations, or a mix. Default: notes.
    items: Option<Vec<String>>,
    /// For recent: how far back to look.
    #[serde(default = "d7d")]
    since: String,
    /// For create: not supported, this server holds one project.
    name: Option<String>,
    /// For create: accepted for compatibility and ignored.
    path: Option<String>,
    /// For create: accepted for compatibility and ignored.
    #[serde(default)]
    set_default: bool,
    /// Accepted for compatibility and ignored: there are no workspaces.
    workspace: Option<String>,
    /// Page number, from 1.
    #[serde(default = "d1")]
    page: usize,
    /// Results per page.
    #[serde(default = "d10")]
    page_size: usize,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SchemaCommand {
    Validate,
    Infer,
    Diff,
}

#[derive(Deserialize, JsonSchema)]
pub struct SchemaArgs {
    /// validate checks notes against their schema; infer suggests one from existing notes; diff compares the two.
    command: SchemaCommand,
    /// The project name. Optional: this server holds one project, used when this is omitted.
    #[serde(default)]
    project: String,
    /// The frontmatter `type:` to check. Required for infer and diff.
    note_type: Option<String>,
    /// For validate: one note, instead of a whole note type.
    identifier: Option<String>,
    /// For infer: the share of notes a field must appear in to be suggested.
    #[serde(default = "d025")]
    threshold: f64,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

fn d025() -> f64 {
    0.25
}

#[derive(Deserialize, JsonSchema)]
pub struct ManArgs {
    /// The page to read: a tool name such as 'mem_search' or 'mem-search(3)'.
    page: Option<String>,
    /// Words to find in the manual; lists the pages that mention them all.
    query: Option<String>,
    /// text (readable, the default) or json (structured, for programs).
    #[serde(default)]
    output_format: Format,
}

/// One ranked search result.
enum Found {
    Note(NoteRow, Hit),
    Obs(ObsRow, NoteRow, f64),
    Rel(RelRow, NoteRow, Option<NoteRow>, f64),
}

const WRITE_TOOLS: [&str; 4] = ["mem_write", "mem_edit", "mem_move", "mem_delete"];

#[tool_router]
impl Server {
    pub fn new(ctx: Arc<Ctx>) -> Self {
        let mut tool_router = Self::tool_router();
        if ctx.read_only {
            for name in WRITE_TOOLS {
                tool_router.remove_route(name);
            }
        }
        Self { ctx, tool_router }
    }

    fn store(&self) -> &Store {
        &self.ctx.store
    }

    fn check_project(&self, p: &str) -> Result<(), Fail> {
        let name = &self.store().project;
        let p = p.trim();
        if p.is_empty() || p == name || p.rsplit('/').next() == Some(name) || self.ctx.project_id.as_deref() == Some(p) {
            return Ok(());
        }
        fail(format!("Unknown project {p:?}; this server holds one project, {name:?}"))
    }

    fn after_write(&self, rel: &str) -> Result<(), Fail> {
        self.store().index_file(rel)?;
        self.ctx.vectors.mark_stale();
        Ok(())
    }

    fn note_text(&self, n: &NoteRow) -> Result<String, Fail> {
        Ok(std::fs::read_to_string(self.store().abs(&n.path))?)
    }

    fn guard_checksum(&self, n: &NoteRow, expected: &Option<String>) -> Result<(), Fail> {
        if let Some(want) = expected {
            let bytes = std::fs::read(self.store().abs(&n.path))?;
            let have = checksum(&bytes);
            if &have != want {
                return fail(format!("{} changed since it was read (checksum is now {have}); read it again", n.path));
            }
        }
        Ok(())
    }

    async fn query_vector(&self, q: &str) -> Option<Vec<f32>> {
        let e = self.ctx.embedder.as_ref()?;
        match e.embed_query(q).await {
            Ok(v) => Some(v),
            Err(err) => {
                log::warn!("query embedding failed, searching by text only: {err:#}");
                None
            }
        }
    }

    #[tool(
        description = "Search the knowledge base, ranked by relevance. `query` supports quoted phrases, AND/OR/NOT and tag:name. `mode` picks hybrid (meaning and words fused; the default when embeddings are configured), text, semantic, title or permalink (globs with *). Narrow with `filters` (frontmatter), `note_types`, `items` (note, observation, relation), `categories` and `since`. `lines` returns each hit's matching lines with context, like grep. `path` limits the search to a folder; `identifier` searches one note. Read full notes with mem_read."
    )]
    async fn mem_search(&self, Parameters(a): Parameters<SearchArgs>) -> R {
        if let Some(p) = &a.project {
            self.check_project(p)?;
        }
        for p in a.projects.iter().flatten() {
            self.check_project(p)?;
        }
        if a.time_kind.is_some() || a.valid_at.is_some() || a.valid_overlaps.is_some() {
            return fail("time_kind, valid_at and valid_overlaps are not supported by this server");
        }
        let store = self.store();
        let query = a.query.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let q = search::parse_query(query.unwrap_or(""));
        // A query of tags alone ("tag:coffee") has no words to match: search by the tag filter only.
        let query = query.filter(|_| q.all.is_some() || q.tags.is_empty());
        let mut scope = Scope {
            path: a.path.clone(),
            note_types: a.note_types.clone(),
            since: a.since.as_deref().map(|s| search::parse_since(s, now())).transpose()?,
            filters: a.filters.clone(),
            tags: q.tags.clone(),
            only: None,
        };
        if let Some(id) = &a.identifier {
            let n = store.require(id)?;
            scope.only = Some(n.id);
            return self.search_one_note(&n, &q, a.lines.unwrap_or(0).min(10), a.max_matches, a.output_format);
        }
        let allowed = scope.allowed(store)?;
        let semantic_ready = self.ctx.embedder.is_some();
        let mode = Mode::parse(a.mode.as_deref(), semantic_ready)?;
        if mode == Mode::Semantic && !semantic_ready {
            return fail("Semantic search needs an embedding server (EMBED_URL); use mode='text'");
        }
        let mut items =
            a.items.clone().unwrap_or_else(|| if a.categories.is_some() { vec!["observation".into()] } else { vec!["note".into()] });
        items.dedup();
        let min_sim = a.min_similarity.unwrap_or(search::DEFAULT_MIN_SIMILARITY);
        let wants_vector = query.is_some() && matches!(mode, Mode::Hybrid | Mode::Semantic) && q.all.is_some();
        let qv = if wants_vector { self.query_vector(query.unwrap_or("")).await } else { None };
        if mode == Mode::Semantic && qv.is_none() && wants_vector {
            return fail("The embedding server did not answer; try mode='text'");
        }
        let entries = match &qv {
            Some(_) => Some(self.ctx.vectors.get(store, self.ctx.embedder.as_ref().map(|e| e.model.as_str()).unwrap_or(""))?),
            None => None,
        };
        let pool = search::pool().max(a.page * a.page_size.clamp(1, 100));
        let demoted = store.observations_in(&self.ctx.demoted)?;
        let mut found: Vec<Found> = Vec::new();
        for kind in &items {
            match kind.as_str() {
                "note" | "notes" | "entity" => {
                    let hits = match (query, mode) {
                        (None, _) => {
                            let mut notes: Vec<NoteRow> =
                                store.all_notes()?.into_iter().filter(|n| allowed.as_ref().is_none_or(|s| s.contains(&n.id))).collect();
                            notes.sort_by_key(|n| std::cmp::Reverse(n.updated));
                            notes.into_iter().map(|n| Hit { id: n.id, score: 1.0, excerpt: None, chunk: None }).collect()
                        }
                        (Some(qs), Mode::Title | Mode::Permalink) => self.name_hits(qs, mode, &allowed)?,
                        (Some(_), Mode::Text) => search::text_notes(store, &q, &allowed, pool)?.0,
                        (Some(_), Mode::Semantic) => search::semantic(
                            entries.as_deref().map(Vec::as_slice).unwrap_or(&[]),
                            qv.as_deref().unwrap_or(&[]),
                            false,
                            &allowed,
                            &demoted,
                            min_sim,
                            pool,
                        ),
                        (Some(_), Mode::Hybrid) => match (&entries, &qv) {
                            (Some(e), Some(v)) => search::fuse(
                                search::text_chunks(store, &q, &allowed, pool)?,
                                search::semantic(e, v, false, &allowed, &demoted, min_sim, pool),
                            ),
                            _ => search::text_notes(store, &q, &allowed, pool)?.0,
                        },
                    };
                    for h in hits {
                        if let Some(n) = store.note_by_id(h.id)? {
                            found.push(Found::Note(n, h));
                        }
                    }
                }
                "observation" | "observations" => {
                    let hits: Vec<Hit> = match query {
                        None => store
                            .observations_since(scope.since.unwrap_or(0))?
                            .into_iter()
                            .filter(|o| allowed.as_ref().is_none_or(|s| s.contains(&o.note_id)))
                            .map(|o| Hit { id: o.id, score: 1.0, excerpt: None, chunk: None })
                            .collect(),
                        Some(_) => {
                            let text = if mode == Mode::Semantic {
                                (vec![], true)
                            } else {
                                let (hits, strict) = search::text_observations(store, &q, pool * 4)?;
                                (search::demote(hits, &demoted), strict)
                            };
                            match (&entries, &qv, mode) {
                                (Some(e), Some(v), Mode::Hybrid | Mode::Semantic) => {
                                    search::fuse(text, search::semantic(e, v, true, &allowed, &demoted, min_sim, pool))
                                }
                                _ => text.0,
                            }
                        }
                    };
                    for h in hits {
                        let Some(o) = store.observation(h.id)? else { continue };
                        if allowed.as_ref().is_some_and(|s| !s.contains(&o.note_id)) {
                            continue;
                        }
                        if let Some(cats) = &a.categories
                            && !cats.iter().any(|c| o.category.as_deref().is_some_and(|oc| search::fold(oc) == search::fold(c)))
                        {
                            continue;
                        }
                        if let Some(n) = store.note_by_id(o.note_id)? {
                            found.push(Found::Obs(o, n, h.score));
                        }
                    }
                }
                "relation" | "relations" => {
                    let notes: BTreeMap<i64, NoteRow> = store.all_notes()?.into_iter().map(|n| (n.id, n)).collect();
                    let mut rels: Vec<(f64, RelRow)> = Vec::new();
                    for r in store.all_relations()? {
                        if allowed.as_ref().is_some_and(|s| !s.contains(&r.note_id)) {
                            continue;
                        }
                        let from = notes.get(&r.note_id).map(|n| n.title.as_str()).unwrap_or("");
                        let to = r.target_id.and_then(|t| notes.get(&t)).map(|n| n.title.as_str()).unwrap_or(&r.target);
                        let hay = search::fold(&format!("{from} {} {to}", r.rel_type));
                        let score =
                            if q.words.is_empty() { 1.0 } else { q.words.iter().filter(|w| hay.contains(w.as_str())).count() as f64 };
                        if score > 0.0 {
                            rels.push((score, r));
                        }
                    }
                    rels.sort_by(|a, b| b.0.total_cmp(&a.0));
                    for (score, r) in rels {
                        if let Some(from) = notes.get(&r.note_id) {
                            let to = r.target_id.and_then(|t| notes.get(&t)).cloned();
                            found.push(Found::Rel(r, from.clone(), to, score));
                        }
                    }
                }
                other => return bad_args(format!("unknown item kind {other:?}; use note, observation or relation")),
            }
        }
        let (page, total) = paginate(found, a.page, a.page_size);
        self.render_search(page, total, &a, mode, query)
    }

    fn name_hits(&self, qs: &str, mode: Mode, allowed: &Option<HashSet<i64>>) -> Result<Vec<Hit>, Fail> {
        let prefix = format!("{}/", self.store().project);
        let qf = search::fold(qs);
        let mut out: Vec<Hit> = Vec::new();
        for n in self.store().all_notes()? {
            if allowed.as_ref().is_some_and(|s| !s.contains(&n.id)) {
                continue;
            }
            let field = if mode == Mode::Title { n.title.clone() } else { n.permalink.clone() };
            let bare = field.strip_prefix(&prefix).unwrap_or(&field).to_string();
            let score = if qs.contains('*') || qs.contains('?') {
                if search::glob(qs, &field) || search::glob(qs, &bare) { 1.0 } else { 0.0 }
            } else {
                let f = search::fold(&field);
                if f == qf || search::fold(&bare) == qf {
                    3.0
                } else if f.starts_with(&qf) {
                    2.0
                } else if qf.split_whitespace().all(|w| f.contains(w)) {
                    1.0
                } else {
                    0.0
                }
            };
            if score > 0.0 {
                out.push(Hit { id: n.id, score, excerpt: None, chunk: None });
            }
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    fn search_one_note(&self, n: &NoteRow, q: &search::Query, context: usize, max: usize, format: Format) -> R {
        let body = self.note_text(n)?;
        let blocks = if q.words.is_empty() { vec![] } else { search::grep(&body, &q.words, context, max.clamp(1, 100)) };
        if format == Format::Json {
            return json_out(
                json!({ "note": note_json(n), "matches": blocks.iter().map(|(l, t)| json!({"line": l, "text": t})).collect::<Vec<_>>() }),
            );
        }
        if blocks.is_empty() {
            return text(format!("No matching lines in {} ({}).", n.title, n.path));
        }
        let mut out = format!("{} ({}): {} matching block(s)\n", n.title, n.path, blocks.len());
        for (l, t) in blocks {
            out.push_str(&format!("\n{}:{l}\n{t}\n", n.path));
        }
        text(out)
    }

    fn excerpt(&self, h: &Hit) -> Option<String> {
        if let Some(e) = h.excerpt.as_ref().filter(|e| !e.trim().is_empty()) {
            return Some(e.replace('\n', " "));
        }
        let t = h.chunk.and_then(|c| self.store().chunk_text(c).ok().flatten())?;
        let body = t.split_once("\n\n").map(|(_, b)| b).unwrap_or(&t);
        Some(cut(&body.replace('\n', " "), 280))
    }

    fn render_search(&self, page: Vec<Found>, total: usize, a: &SearchArgs, mode: Mode, query: Option<&str>) -> R {
        let size = a.page_size.clamp(1, 100);
        let lines_ctx = a.lines.map(|l| l.min(10));
        let words = search::parse_query(query.unwrap_or("")).words;
        let mode_name = format!("{mode:?}").to_lowercase();
        if a.output_format == Format::Json {
            let mut results = Vec::new();
            for f in &page {
                results.push(match f {
                    Found::Note(n, h) => {
                        let mut v = note_json(n);
                        v["kind"] = json!("note");
                        v["score"] = json!((h.score * 1000.0).round() / 1000.0);
                        if !a.compact {
                            match lines_ctx {
                                Some(c) => {
                                    let body = self.note_text(n).unwrap_or_default();
                                    v["matches"] = json!(search::grep(&body, &words, c, a.max_matches).into_iter().map(|(l, t)| json!({"line": l, "text": t})).collect::<Vec<_>>());
                                }
                                None => v["excerpt"] = json!(self.excerpt(h)),
                            }
                        }
                        v
                    }
                    Found::Obs(o, n, s) => json!({"kind": "observation", "note": n.title, "path": n.path, "permalink": n.permalink, "score": s, "observation": obs_json(o)}),
                    Found::Rel(r, from, to, s) => json!({
                        "kind": "relation", "from": from.title, "from_path": from.path, "relation_type": r.rel_type,
                        "to": to.as_ref().map(|t| t.title.clone()).unwrap_or(r.target.clone()), "to_path": to.as_ref().map(|t| t.path.clone()), "resolved": to.is_some(), "score": s,
                    }),
                });
            }
            return json_out(json!({ "results": results, "mode": mode_name, "page": a.page, "page_size": size, "total": total }));
        }
        if page.is_empty() {
            let hint = if total > 0 { " on this page" } else { "" };
            return text(format!("No results{hint} for {:?} ({mode_name}).", a.query.as_deref().unwrap_or("").trim()));
        }
        let mut out =
            format!("Results for {:?} ({mode_name}, {}):\n", a.query.as_deref().unwrap_or("").trim(), pages_line(a.page, size, total));
        let start = (a.page.max(1) - 1) * size;
        for (i, f) in page.iter().enumerate() {
            let k = start + i + 1;
            match f {
                Found::Note(n, h) => {
                    out.push_str(&format!("\n{k}. {}\n   {} · {} · score {:.3}\n", n.title, n.path, n.note_type, h.score));
                    if a.compact {
                        continue;
                    }
                    match lines_ctx {
                        Some(c) => {
                            let body = self.note_text(n).unwrap_or_default();
                            for (l, t) in search::grep(&body, &words, c, a.max_matches) {
                                out.push_str(&format!("   {l}: {}\n", t.replace('\n', "\n      ")));
                            }
                        }
                        None => {
                            if let Some(e) = self.excerpt(h) {
                                out.push_str(&format!("   {e}\n"));
                            }
                        }
                    }
                }
                Found::Obs(o, n, s) => {
                    out.push_str(&format!("\n{k}. {}\n   {}:{} · {} · score {s:.3}\n", obs_line(o), n.path, o.line, n.title))
                }
                Found::Rel(r, from, to, s) => {
                    let target = to.as_ref().map(|t| t.title.as_str()).unwrap_or(&r.target);
                    let mark = if to.is_some() { "" } else { " (unresolved)" };
                    out.push_str(&format!("\n{k}. {} —{}→ {target}{mark}\n   {} · score {s:.0}\n", from.title, r.rel_type, from.path));
                }
            }
        }
        text(out)
    }

    #[tool(
        description = "Read a note or file by path, permalink, memory:// URL or exact title. Slice a long note with `section` (a heading), `start_line`/`end_line` or `max_tokens`; line numbers count the whole file, frontmatter included. `include_content=false` returns only the metadata and the checksum that mem_write, mem_edit, mem_move and mem_delete take as expected_checksum. Images come back as images. A missing note is an error: find notes with mem_search."
    )]
    async fn mem_read(&self, Parameters(a): Parameters<ReadArgs>) -> R {
        self.check_project(&a.project)?;
        let store = self.store();
        let Some(n) = store.resolve(&a.identifier)? else {
            return self.read_file(&a);
        };
        let raw = self.note_text(&n)?;
        let sum = checksum(raw.as_bytes());
        if !a.include_content {
            let mut v = note_json(&n);
            v["checksum"] = json!(sum);
            if a.output_format == Format::Json {
                return json_out(v);
            }
            return text(format!(
                "title: {}\npath: {}\npermalink: {}\ntype: {}\nchecksum: {sum}\nupdated_at: {}\n",
                n.title,
                n.path,
                n.permalink,
                n.note_type,
                date(n.updated)
            ));
        }
        let lines: Vec<&str> = raw.lines().collect();
        let (body_line, mut start, mut end) = (note::split_frontmatter(&raw).2 + 1, a.start_line, a.end_line);
        if let Some(sec) = &a.section {
            let h = vault::find_section(&lines, sec)?;
            start = Some(h.idx + 1);
            end = Some(vault::section_end(&lines, &h, true));
        }
        let sliced = start.is_some() || end.is_some() || a.max_tokens.is_some();
        let first = if a.include_frontmatter || start.is_some() { 1 } else { body_line };
        let (content, next) =
            if sliced || !a.include_frontmatter { vault::slice(&raw, start, end, a.max_tokens, first) } else { (raw.clone(), None) };
        if a.output_format == Format::Json {
            let mut v = note_json(&n);
            v["checksum"] = json!(sum);
            v["content"] = json!(content);
            v["next_line"] = json!(next);
            return json_out(v);
        }
        let mut out = content;
        if let Some(l) = next {
            out.push_str(&format!("\n\n[… {} continues at line {l}; read again with start_line={l}]", n.path));
        }
        text(out)
    }

    fn read_file(&self, a: &ReadArgs) -> R {
        let rel = inside(self.store(), &a.identifier)?;
        let path = self.store().abs(&rel);
        if !path.is_file() {
            return fail(format!("Note not found: {}. Find notes with mem_search or mem_list.", a.identifier));
        }
        let bytes = std::fs::read(&path)?;
        let ext = rel.rsplit('.').next().unwrap_or("").to_lowercase();
        let image = match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        };
        if let Some(mime) = image {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(CallToolResult::success(vec![ContentBlock::image(b64, mime)]));
        }
        let Ok(s) = String::from_utf8(bytes) else {
            return fail(format!("{rel} is a binary file this server does not return"));
        };
        text(vault::slice(&s, a.start_line, a.end_line, a.max_tokens, 1).0)
    }

    #[tool(
        description = "Create or replace a note. Give a file path as `identifier` ('notes/plan.md'), or `title` plus `directory`. The title comes from the content's frontmatter, then `title`, then the file name. An existing note is an error unless `overwrite=true`; pass `expected_checksum` (from mem_read) to replace only the revision you read. To change part of a note use mem_edit. Content may hold frontmatter, observations ('- [category] fact #tag') and relations ('- relation_type [[Other Note]]'). The result lists observations already saved that say nearly what this write added: update one of those rather than keep both."
    )]
    async fn mem_write(&self, Parameters(a): Parameters<WriteArgs>) -> R {
        self.check_project(&a.project)?;
        let store = self.store();
        let project = store.project.clone();
        let rel = match (&a.identifier, &a.title) {
            (Some(id), _) => with_md(&inside(store, id)?),
            (None, Some(t)) => {
                let dir = inside(store, a.directory.as_deref().unwrap_or("/"))?;
                let file = format!("{}.md", note::file_stem_for_title(t));
                if dir.is_empty() { file } else { format!("{dir}/{file}") }
            }
            (None, None) => return bad_args("give `identifier` (a file path) or `title` (with an optional `directory`)"),
        };
        if rel.is_empty() || rel == ".md" {
            return bad_args("the note needs a file name");
        }
        let path = store.abs(&rel);
        let existing = if path.exists() { store.resolve(&rel)? } else { None };
        if path.exists() {
            let allowed = a.overwrite.unwrap_or(false) || a.expected_checksum.is_some();
            if !allowed {
                return fail(format!("{rel} already exists; pass overwrite=true to replace it, or edit it with mem_edit"));
            }
            if let Some(want) = &a.expected_checksum {
                let have = checksum(&std::fs::read(&path)?);
                if &have != want {
                    return fail(format!("{rel} changed since it was read (checksum is now {have}); read it again"));
                }
            }
        } else if a.expected_checksum.is_some() {
            return fail(format!("{rel} does not exist, so it cannot match expected_checksum"));
        }
        let stem = rel.rsplit('/').next().unwrap_or(&rel).trim_end_matches(".md").to_string();
        let given = vault::frontmatter_of(&a.content);
        let title = note::fm_str(&given, "title").or(a.title.clone()).unwrap_or(stem);
        let permalink = match &existing {
            Some(n) => n.permalink.clone(),
            None => self.unique_permalink(&note::default_permalink(&project, &rel))?,
        };
        let tags = match &a.tags {
            Some(Value::String(s)) => note::split_tags(s),
            Some(Value::Array(v)) => v.iter().filter_map(|t| t.as_str().map(|s| s.trim_start_matches('#').to_string())).collect(),
            _ => vec![],
        };
        let note_type = a.note_type.clone().unwrap_or_else(|| "note".into());
        let composed = vault::compose_new(&a.content, &title, &note_type, &permalink, &tags, a.metadata.as_ref())?;
        let before = self.contents(existing.as_ref())?;
        vault::write_atomic(&path, &composed)?;
        self.after_write(&rel)?;
        let n = store.require(&rel)?;
        let similar = self.similar_within_budget(&n, &before).await;
        self.write_report(if existing.is_some() { "Updated" } else { "Created" }, &n, &similar, a.output_format)
    }

    fn unique_permalink(&self, base: &str) -> Result<String, Fail> {
        let taken: HashSet<String> = self.store().all_notes()?.into_iter().map(|n| n.permalink).collect();
        if !taken.contains(base) {
            return Ok(base.to_string());
        }
        Ok((1..).map(|i| format!("{base}-{i}")).find(|p| !taken.contains(p)).unwrap_or_else(|| base.to_string()))
    }

    /// The observation texts a note holds, to tell afterwards which ones a write added.
    fn contents(&self, n: Option<&NoteRow>) -> Result<HashSet<String>, Fail> {
        Ok(match n {
            Some(n) => self.store().observations(n.id)?.into_iter().map(|o| o.content).collect(),
            None => HashSet::new(),
        })
    }

    /// The repeat check, given up when the embedding server is busy: the write is already done.
    async fn similar_within_budget(&self, n: &NoteRow, before: &HashSet<String>) -> Vec<(ObsRow, NoteRow, f32)> {
        match tokio::time::timeout(REPEAT_CHECK_BUDGET, self.similar_to_added(n, before)).await {
            Ok(found) => found,
            Err(_) => {
                log::info!("duplicate check skipped: over {}s", REPEAT_CHECK_BUDGET.as_secs());
                vec![]
            }
        }
    }

    /// Observations already saved that say nearly what this write added, so the model can update one
    /// instead of repeating it. Best effort: without embeddings, or if the server fails, there are none.
    async fn similar_to_added(&self, n: &NoteRow, before: &HashSet<String>) -> Vec<(ObsRow, NoteRow, f32)> {
        let Some(e) = &self.ctx.embedder else { return vec![] };
        let store = self.store();
        let Ok(now) = store.observations(n.id) else { return vec![] };
        let added: Vec<ObsRow> =
            now.into_iter().filter(|o| !before.contains(&o.content) && worth_comparing(&o.content)).take(MAX_COMPARED).collect();
        if added.is_empty() {
            return vec![];
        }
        let fail = |err: anyhow::Error| {
            log::warn!("duplicate check skipped: {err:#}");
            vec![]
        };
        // Compared on the observations' own text: the stored vectors carry the note title, which pulls
        // repeats in different notes apart, so they only shortlist. Bare vectors are cached under "<model>:bare".
        let bare = |o: &ObsRow| store::observation_text("", o.category.as_deref(), &o.content).trim_start().to_string();
        let bare_model = format!("{}:bare", e.model);
        let added_bare = match self.bare_vectors(e, store, &bare_model, &added.iter().map(bare).collect::<Vec<_>>()).await {
            Ok(v) => v,
            Err(err) => return fail(err),
        };
        let Ok(entries) = self.ctx.vectors.get(store, &e.model) else { return vec![] };
        let added_ids: HashSet<i64> = added.iter().map(|o| o.id).collect();
        let mut pairs: Vec<(usize, ObsRow)> = Vec::new();
        for (i, v) in added_bare.iter().enumerate() {
            let mut near: Vec<(f32, i64)> = entries
                .iter()
                .filter_map(|en| en.obs_id.filter(|id| !added_ids.contains(id)).map(|id| (embed::dot(&en.vec, v), id)))
                .filter(|(s, _)| *s >= SHORTLIST)
                .collect();
            near.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, id) in near.into_iter().take(SHORTLIST_SIZE) {
                if let Ok(Some(o)) = store.observation(id)
                    && worth_comparing(&o.content)
                    && same_numbers(&added[i].content, &o.content)
                {
                    pairs.push((i, o));
                }
            }
        }
        pairs.truncate(SHORTLIST_TOTAL);
        if pairs.is_empty() {
            return vec![];
        }
        let vecs = match self.bare_vectors(e, store, &bare_model, &pairs.iter().map(|(_, o)| bare(o)).collect::<Vec<_>>()).await {
            Ok(v) => v,
            Err(err) => return fail(err),
        };
        let mut best: BTreeMap<i64, (f32, ObsRow)> = BTreeMap::new();
        for (k, (i, o)) in pairs.into_iter().enumerate() {
            let s = embed::dot(&added_bare[i], &vecs[k]);
            if s >= SAME_FACT && best.get(&o.id).is_none_or(|(b, _)| s > *b) {
                best.insert(o.id, (s, o));
            }
        }
        let mut found: Vec<(f32, ObsRow)> = best.into_values().collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        found
            .into_iter()
            .take(MAX_SIMILAR)
            .filter_map(|(s, o)| store.note_by_id(o.note_id).ok().flatten().map(|note| (o, note, s)))
            .collect()
    }

    /// Vectors for these texts, from the cache where present; the rest are embedded and cached.
    async fn bare_vectors(&self, e: &Embedder, store: &Store, model: &str, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let hashes: Vec<String> = texts.iter().map(|t| checksum(t.as_bytes())).collect();
        let mut have = store.vectors_for(model, &hashes)?;
        let missing: Vec<usize> = (0..texts.len()).filter(|i| !have.contains_key(&hashes[*i])).collect();
        if !missing.is_empty() {
            let fresh = e.embed_now(&missing.iter().map(|i| texts[*i].clone()).collect::<Vec<_>>()).await?;
            let items: Vec<(String, Vec<f32>)> = missing.iter().map(|i| hashes[*i].clone()).zip(fresh).collect();
            store.put_vectors(model, &items)?;
            have.extend(items);
        }
        Ok(hashes.iter().map(|h| have[h].clone()).collect())
    }

    fn write_report(&self, verb: &str, n: &NoteRow, similar: &[(ObsRow, NoteRow, f32)], format: Format) -> R {
        let obs = self.store().observations(n.id)?.len();
        let rels = self.store().outgoing(n.id)?;
        let unresolved: Vec<&str> = rels.iter().filter(|r| r.target_id.is_none()).map(|r| r.target.as_str()).collect();
        if format == Format::Json {
            let mut v = note_json(n);
            v["action"] = json!(verb.to_lowercase());
            v["observations"] = json!(obs);
            v["relations"] = json!(rels.len());
            v["unresolved_relations"] = json!(unresolved);
            v["similar_observations"] = json!(similar
                .iter()
                .map(|(o, note, s)| json!({"note": note.title, "path": note.path, "category": o.category, "content": o.content, "line": o.line, "similarity": s}))
                .collect::<Vec<_>>());
            return json_out(v);
        }
        let mut out = format!(
            "{verb} {}\npermalink: {}\nchecksum: {}\nobservations: {obs}, relations: {}\n",
            n.path,
            n.permalink,
            n.checksum,
            rels.len()
        );
        if !unresolved.is_empty() {
            out.push_str(&format!("unresolved relations (no such note yet): {}\n", unresolved.join(", ")));
        }
        if !similar.is_empty() {
            out.push_str("\nAlready saved, nearly the same (update one of these rather than keeping both):\n");
            for (o, note, s) in similar {
                let cat = o.category.as_deref().map(|c| format!("[{c}] ")).unwrap_or_default();
                out.push_str(&format!("- {cat}{} ({}:{}, similarity {s:.2})\n", o.content, note.path, o.line));
            }
        }
        text(out)
    }

    #[tool(
        description = "Change part of an existing note. `command`: append or prepend `content`; find_replace `find_text` with `content` (exact text that must occur `expected_replacements` times, default 1); replace_section, insert_before_section or insert_after_section for a `section` heading ('Decisions', 'Auth/Decisions', 'Decisions[1]'); set_metadata to merge `metadata` into the frontmatter without touching the body. `metadata` also merges with any other command. Pass `expected_checksum` (from mem_read) to edit only the revision you read. Like mem_write, the result lists observations already saved that say nearly what the edit added. Create notes with mem_write."
    )]
    async fn mem_edit(&self, Parameters(a): Parameters<EditArgs>) -> R {
        self.check_project(&a.project)?;
        let n = self.store().require(&a.identifier)?;
        self.guard_checksum(&n, &a.expected_checksum)?;
        let raw = self.note_text(&n)?;
        let content = a.content.as_deref();
        let need = |what: &str| -> Result<&str, Fail> {
            match content {
                Some(c) => Ok(c),
                None => bad_args(format!("{what} needs `content`")),
            }
        };
        let section = || -> Result<&str, Fail> {
            match a.section.as_deref() {
                Some(s) if !s.trim().is_empty() => Ok(s),
                _ => bad_args("section commands need `section`"),
            }
        };
        let mut edited = match a.command {
            EditCommand::Append => vault::apply(&raw, Edit::Append(need("append")?))?,
            EditCommand::Prepend => vault::apply(&raw, Edit::Prepend(need("prepend")?))?,
            EditCommand::FindReplace => {
                let Some(find) = a.find_text.as_deref() else { return bad_args("find_replace needs `find_text`") };
                vault::apply(&raw, Edit::FindReplace { find, replace: need("find_replace")?, expected: a.expected_replacements.max(1) })?
            }
            EditCommand::ReplaceSection => vault::apply(
                &raw,
                Edit::ReplaceSection { section: section()?, content: need("replace_section")?, subsections: a.replace_subsections },
            )?,
            EditCommand::InsertBeforeSection => {
                vault::apply(&raw, Edit::InsertBefore { section: section()?, content: need("insert_before_section")? })?
            }
            EditCommand::InsertAfterSection => {
                vault::apply(&raw, Edit::InsertAfter { section: section()?, content: need("insert_after_section")? })?
            }
            EditCommand::SetMetadata => {
                if a.metadata.is_none() {
                    return bad_args("set_metadata needs `metadata`");
                }
                raw.clone()
            }
        };
        if let Some(m) = &a.metadata {
            let mut fm = vault::frontmatter_of(&edited);
            vault::merge_metadata(&mut fm, m);
            edited = vault::with_frontmatter(&edited, &fm)?;
        }
        let before = self.contents(Some(&n))?;
        vault::write_atomic(&self.store().abs(&n.path), &edited)?;
        self.after_write(&n.path)?;
        let n = self.store().require(&n.path)?;
        let similar = self.similar_within_budget(&n, &before).await;
        self.write_report("Edited", &n, &similar, a.output_format)
    }

    #[tool(
        description = "Move or rename a note (`identifier`) or a directory (`path`) within the project. `destination` is the new project-relative path; ending it with '/' moves into that folder under the current name. Pass `expected_checksum` (from mem_read) to move a note only if it is still the revision you read. Links by title keep resolving; the permalink in the frontmatter does not change."
    )]
    async fn mem_move(&self, Parameters(a): Parameters<MoveArgs>) -> R {
        self.check_project(&a.project)?;
        let store = self.store();
        let dest_raw = a.destination.trim();
        let into_folder = dest_raw.ends_with('/');
        let dest = inside(store, dest_raw)?;
        let (from, to) = match (&a.identifier, &a.path) {
            (Some(id), None) => {
                let n = store.require(id)?;
                self.guard_checksum(&n, &a.expected_checksum)?;
                let file = n.path.rsplit('/').next().unwrap_or(&n.path).to_string();
                let to = if into_folder || dest.is_empty() {
                    if dest.is_empty() { file } else { format!("{dest}/{file}") }
                } else {
                    with_md(&dest)
                };
                (n.path, to)
            }
            (None, Some(p)) => {
                let from = inside(store, p)?;
                if from.is_empty() || !store.abs(&from).is_dir() {
                    return fail(format!("{p:?} is not a directory of the project"));
                }
                let name = from.rsplit('/').next().unwrap_or(&from).to_string();
                let to = if into_folder { if dest.is_empty() { name } else { format!("{dest}/{name}") } } else { dest.clone() };
                if to.starts_with(&format!("{from}/")) {
                    return fail("a directory cannot move inside itself");
                }
                (from, to)
            }
            _ => return bad_args("give exactly one of `identifier` (a note) or `path` (a directory)"),
        };
        if to.is_empty() || store.abs(&to).exists() {
            return fail(format!("{to:?} already exists or is not a valid destination"));
        }
        if let Some(parent) = store.abs(&to).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(store.abs(&from), store.abs(&to))?;
        store.sync()?;
        self.ctx.vectors.mark_stale();
        if a.output_format == Format::Json {
            return json_out(json!({"moved": from, "to": to}));
        }
        text(format!("Moved {from} → {to}"))
    }

    #[tool(
        description = "Delete a note (`identifier`) or a directory (`path`). A directory delete removes every file under it and needs `recursive=true`. Pass `expected_checksum` (from mem_read) to delete a note only if it is still the revision you read. A note that does not exist is an error. This cannot be undone."
    )]
    async fn mem_delete(&self, Parameters(a): Parameters<DeleteArgs>) -> R {
        self.check_project(&a.project)?;
        let store = self.store();
        let what = match (&a.identifier, &a.path) {
            (Some(id), None) => {
                let n = store.require(id)?;
                self.guard_checksum(&n, &a.expected_checksum)?;
                std::fs::remove_file(store.abs(&n.path))?;
                n.path
            }
            (None, Some(p)) => {
                let rel = inside(store, p)?;
                if rel.is_empty() {
                    return fail("refusing to delete the project root");
                }
                if !store.abs(&rel).is_dir() {
                    return fail(format!("{p:?} is not a directory of the project"));
                }
                if !a.recursive {
                    return fail(format!("{rel} is a directory; pass recursive=true to delete it and everything in it"));
                }
                std::fs::remove_dir_all(store.abs(&rel))?;
                format!("{rel}/")
            }
            _ => return bad_args("give exactly one of `identifier` (a note) or `path` (a directory)"),
        };
        store.sync()?;
        self.ctx.vectors.mark_stale();
        if a.output_format == Format::Json {
            return json_out(json!({"deleted": what}));
        }
        text(format!("Deleted {what}"))
    }

    #[tool(
        description = "List one folder of the project: its subfolders, then its files with title, date and permalink. '/' lists the root. Filter files with `glob` and order them with `sort`. For a recursive search by name or frontmatter, use mem_find."
    )]
    async fn mem_list(&self, Parameters(a): Parameters<ListArgs>) -> R {
        self.check_project(&a.project)?;
        let store = self.store();
        let dir = inside(store, &a.path)?;
        let abs = if dir.is_empty() { store.root.clone() } else { store.abs(&dir) };
        if !abs.is_dir() {
            return fail(format!("{:?} is not a folder of the project", a.path));
        }
        let mut folders = Vec::new();
        let mut files = Vec::new();
        for e in std::fs::read_dir(&abs)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                folders.push(rel);
            } else if a.glob.as_ref().is_none_or(|g| search::glob(g, &name)) {
                let row = store.resolve(&rel).ok().flatten().filter(|n| n.path == rel);
                files.push((rel, row));
            }
        }
        folders.sort();
        let key = |r: &(String, Option<NoteRow>)| {
            (
                r.1.as_ref().map(|n| n.title.to_lowercase()).unwrap_or_else(|| r.0.to_lowercase()),
                r.1.as_ref().map(|n| n.updated).unwrap_or(0),
            )
        };
        match a.sort.unwrap_or(Sort::TitleAsc) {
            Sort::TitleAsc => files.sort_by_key(|r| key(r).0),
            Sort::TitleDesc => files.sort_by_key(|r| std::cmp::Reverse(key(r).0)),
            Sort::UpdatedAsc => files.sort_by_key(|r| key(r).1),
            Sort::UpdatedDesc => files.sort_by_key(|r| std::cmp::Reverse(key(r).1)),
        }
        let entries: Vec<Value> = folders
            .iter()
            .map(|f| json!({"kind": "folder", "path": format!("{f}/")}))
            .chain(files.iter().map(|(rel, n)| match n {
                Some(n) => json!({"kind": "note", "path": rel, "title": n.title, "updated_at": date(n.updated), "permalink": n.permalink}),
                None => json!({"kind": "file", "path": rel}),
            }))
            .collect();
        let (page, total) = paginate(entries, a.page, a.page_size);
        if a.output_format == Format::Json {
            return json_out(json!({"path": format!("/{dir}"), "entries": page, "page": a.page, "total": total}));
        }
        let mut out = format!("/{dir} ({}):\n", pages_line(a.page, a.page_size, total));
        for e in &page {
            match e["kind"].as_str() {
                Some("folder") => out.push_str(&format!("  {}\n", e["path"].as_str().unwrap_or(""))),
                Some("note") => out.push_str(&format!(
                    "  {} — {} ({}, {})\n",
                    e["path"].as_str().unwrap_or(""),
                    e["title"].as_str().unwrap_or(""),
                    e["updated_at"].as_str().unwrap_or(""),
                    e["permalink"].as_str().unwrap_or("")
                )),
                _ => out.push_str(&format!("  {}\n", e["path"].as_str().unwrap_or(""))),
            }
        }
        text(out)
    }

    #[tool(
        description = "Find files in the project, recursively. With `name`, match file names against a glob ('*meeting*'). With `filters`, match notes by frontmatter, e.g. {\"status\": \"active\"} or {\"priority\": {\"$in\": [\"high\"]}}, and with `fields` return just those frontmatter values per note. `path` limits the search to a folder. For ranked text or meaning search, use mem_search."
    )]
    async fn mem_find(&self, Parameters(a): Parameters<FindArgs>) -> R {
        self.check_project(&a.project)?;
        if a.name.is_some() && a.filters.is_some() {
            return bad_args("use `name` or `filters`, not both");
        }
        let store = self.store();
        let dir = inside(store, &a.path)?;
        let depth = a.depth.clamp(1, 10);
        let mut rows: Vec<Value> = Vec::new();
        for rel in store::walk_all(&store.root) {
            let inside = dir.is_empty() || rel.starts_with(&format!("{dir}/"));
            let below = rel.strip_prefix(&format!("{dir}/")).unwrap_or(&rel);
            if !inside || below.matches('/').count() >= depth {
                continue;
            }
            let file = rel.rsplit('/').next().unwrap_or(&rel);
            if let Some(g) = &a.name
                && !search::glob(g, file)
            {
                continue;
            }
            let n = if rel.ends_with(".md") { store.resolve(&rel)?.filter(|n| n.path == rel) } else { None };
            if let Some(f) = &a.filters {
                let Some(n) = &n else { continue };
                if !search::matches_filters(&n.frontmatter, f)? {
                    continue;
                }
            }
            let mut v = match &n {
                Some(n) => json!({"path": rel, "title": n.title, "type": n.note_type, "updated_at": date(n.updated)}),
                None => json!({"path": rel}),
            };
            if let (Some(fields), Some(n)) = (&a.fields, &n) {
                let mut m = Map::new();
                for f in fields {
                    m.insert(f.clone(), f.split('.').try_fold(&n.frontmatter, |v, k| v.get(k)).cloned().unwrap_or(Value::Null));
                }
                v["fields"] = Value::Object(m);
            }
            rows.push(v);
        }
        let (page, total) = paginate(rows, a.page, a.page_size);
        if a.output_format == Format::Json {
            return json_out(json!({"results": page, "page": a.page, "total": total}));
        }
        if page.is_empty() {
            return text("No files found.");
        }
        let mut out = format!("Found ({}):\n", pages_line(a.page, a.page_size, total));
        for v in &page {
            out.push_str(&format!("  {}", v["path"].as_str().unwrap_or("")));
            if let Some(t) = v["title"].as_str() {
                out.push_str(&format!(" — {t}"));
            }
            if let Some(f) = v.get("fields") {
                out.push_str(&format!(" {f}"));
            }
            out.push('\n');
        }
        text(out)
    }

    #[tool(
        description = "Follow the knowledge graph from a note: the note, its observations, and the notes its relations reach (both directions), up to `depth` hops (1-5). `identifier` is a path, permalink, memory:// URL, or a pattern such as 'specs/*'. `since` keeps only related notes changed recently. `compact` returns the graph without note bodies; read chosen notes with mem_read."
    )]
    async fn mem_context(&self, Parameters(a): Parameters<ContextArgs>) -> R {
        self.check_project(&a.project)?;
        let store = self.store();
        let prefix = format!("{}/", store.project);
        let pattern = a.identifier.trim().trim_start_matches("memory://");
        let primaries: Vec<NoteRow> = if pattern.contains('*') {
            let p = pattern.strip_prefix(&prefix).unwrap_or(pattern);
            store
                .all_notes()?
                .into_iter()
                .filter(|n| search::glob(p, &n.path) || search::glob(p, n.permalink.strip_prefix(&prefix).unwrap_or(&n.permalink)))
                .collect()
        } else {
            vec![store.require(&a.identifier)?]
        };
        if primaries.is_empty() {
            return fail(format!("No notes match {:?}", a.identifier));
        }
        let since = a.since.as_deref().map(|s| search::parse_since(s, now())).transpose()?.unwrap_or(0);
        let (page, total) = paginate(primaries, a.page, a.page_size.clamp(1, 50));
        let mut results = Vec::new();
        for n in &page {
            let observations = if a.compact { vec![] } else { store.observations(n.id)? };
            let content = if a.compact { None } else { Some(self.note_text(n)?) };
            let mut seen: HashSet<i64> = HashSet::from([n.id]);
            let mut queue: VecDeque<(i64, usize)> = VecDeque::from([(n.id, 0)]);
            let mut related: Vec<Value> = Vec::new();
            let mut edges: Vec<Value> = Vec::new();
            while let Some((id, d)) = queue.pop_front() {
                if d >= a.depth.clamp(1, 5) {
                    continue;
                }
                let mut links: Vec<(RelRow, Option<i64>, &str)> =
                    store.outgoing(id)?.into_iter().map(|r| (r.clone(), r.target_id, "out")).collect();
                links.extend(store.incoming(id)?.into_iter().map(|r| (r.clone(), Some(r.note_id), "in")));
                for (r, other, dir) in links {
                    let from_title = store.note_by_id(r.note_id)?.map(|x| x.title).unwrap_or_default();
                    let to_title =
                        r.target_id.and_then(|t| store.note_by_id(t).ok().flatten()).map(|x| x.title).unwrap_or(r.target.clone());
                    if d == 0 {
                        edges.push(json!({"from": from_title, "relation_type": r.rel_type, "to": to_title, "direction": dir, "resolved": r.target_id.is_some()}));
                    }
                    let Some(other) = other else { continue };
                    if !seen.insert(other) {
                        continue;
                    }
                    let Some(o) = store.note_by_id(other)? else { continue };
                    if o.updated >= since && related.len() < a.max_related.min(100) {
                        related.push(json!({"title": o.title, "path": o.path, "type": o.note_type, "updated_at": date(o.updated), "via": r.rel_type, "depth": d + 1}));
                    }
                    queue.push_back((other, d + 1));
                }
            }
            results.push(json!({
                "note": note_json(n),
                "content": content,
                "observations": observations.iter().map(obs_json).collect::<Vec<_>>(),
                "relations": edges,
                "related": related,
            }));
        }
        if a.output_format == Format::Json {
            return json_out(json!({"results": results, "page": a.page, "total": total, "since": a.since.as_ref().map(|_| date(since))}));
        }
        let mut out = String::new();
        for r in &results {
            let n = &r["note"];
            out.push_str(&format!(
                "# {}\n{} · {} · updated {}\n",
                n["title"].as_str().unwrap_or(""),
                n["path"].as_str().unwrap_or(""),
                n["type"].as_str().unwrap_or(""),
                n["updated_at"].as_str().unwrap_or("")
            ));
            if let Some(c) = r["content"].as_str() {
                out.push_str(&format!("\n{}\n", note::split_frontmatter(c).1.trim()));
            }
            let edges = r["relations"].as_array().cloned().unwrap_or_default();
            if !edges.is_empty() {
                out.push_str("\n## Relations\n");
                for e in &edges {
                    let mark = if e["resolved"].as_bool() == Some(false) { " (unresolved)" } else { "" };
                    out.push_str(&format!(
                        "- {} —{}→ {}{mark}\n",
                        e["from"].as_str().unwrap_or(""),
                        e["relation_type"].as_str().unwrap_or(""),
                        e["to"].as_str().unwrap_or("")
                    ));
                }
            }
            let related = r["related"].as_array().cloned().unwrap_or_default();
            match &a.since {
                Some(_) => out.push_str(&format!("\n## Related, changed since {}\n", date(since))),
                None => out.push_str("\n## Related\n"),
            }
            if related.is_empty() {
                out.push_str("(none)\n");
            }
            for x in &related {
                out.push_str(&format!(
                    "- {} ({}, via {}, depth {}, updated {})\n",
                    x["title"].as_str().unwrap_or(""),
                    x["path"].as_str().unwrap_or(""),
                    x["via"].as_str().unwrap_or(""),
                    x["depth"],
                    x["updated_at"].as_str().unwrap_or("")
                ));
            }
            out.push('\n');
        }
        if total > page.len() {
            out.push_str(&format!("({})\n", pages_line(a.page, a.page_size.clamp(1, 50), total)));
        }
        text(out)
    }

    #[tool(
        description = "Projects. `command`: list (the project this server holds); recent (what changed lately in `project`: `since`, `items`, paging); create is not supported, this server holds one project."
    )]
    async fn mem_project(&self, Parameters(a): Parameters<ProjectArgs>) -> R {
        let store = self.store();
        match a.command {
            ProjectCommand::List => {
                let notes = store.all_notes()?.len();
                let model = self.ctx.embedder.as_ref().map(|e| e.model.clone());
                let (chunks, embedded) = match &model {
                    Some(m) => store.embedding_counts(m)?,
                    None => (0, 0),
                };
                if a.output_format == Format::Json {
                    return json_out(
                        json!({"projects": [{"name": store.project, "id": self.ctx.project_id, "notes": notes, "default": true,
                        "embedding_model": model, "chunks": chunks, "embedded": embedded}]}),
                    );
                }
                let id = self.ctx.project_id.as_deref().map(|i| format!(" [{i}]")).unwrap_or_default();
                let sem = match &model {
                    Some(m) => format!(", semantic search with {m} ({embedded}/{chunks} chunks embedded)"),
                    None => ", text search only".into(),
                };
                text(format!("Projects:\n- {} (default){id}: {notes} notes{sem}\n", store.project))
            }
            ProjectCommand::Create => {
                let _ = (&a.name, &a.path, a.set_default, &a.workspace);
                fail("This server holds one project; creating projects is not supported")
            }
            ProjectCommand::Recent => {
                let Some(p) = &a.project else { return bad_args("recent needs `project`") };
                self.check_project(p)?;
                let since = search::parse_since(&a.since, now())?;
                let items = a.items.clone().unwrap_or_else(|| vec!["note".into()]);
                let mut rows: Vec<(i64, Value, String)> = Vec::new();
                let notes: Vec<NoteRow> = store.all_notes()?.into_iter().filter(|n| n.updated >= since).collect();
                for kind in &items {
                    match kind.as_str() {
                        "note" | "notes" => rows.extend(notes.iter().map(|n| {
                            let mut v = note_json(n);
                            v["kind"] = json!("note");
                            (n.updated, v, format!("{} — {} ({})", date(n.updated), n.title, n.path))
                        })),
                        "observation" | "observations" => {
                            for o in store.observations_since(since)? {
                                let Some(n) = notes.iter().find(|n| n.id == o.note_id) else { continue };
                                rows.push((
                                    n.updated,
                                    json!({"kind": "observation", "note": n.title, "path": n.path, "observation": obs_json(&o)}),
                                    format!("{} — {} · {}", date(n.updated), obs_line(&o), n.title),
                                ));
                            }
                        }
                        "relation" | "relations" => {
                            for n in &notes {
                                for r in store.outgoing(n.id)? {
                                    rows.push((
                                        n.updated,
                                        json!({"kind": "relation", "from": n.title, "relation_type": r.rel_type, "to": r.target}),
                                        format!("{} — {} —{}→ {}", date(n.updated), n.title, r.rel_type, r.target),
                                    ));
                                }
                            }
                        }
                        other => return bad_args(format!("unknown item kind {other:?}")),
                    }
                }
                rows.sort_by_key(|r| std::cmp::Reverse(r.0));
                let (page, total) = paginate(rows, a.page, a.page_size);
                if a.output_format == Format::Json {
                    return json_out(
                        json!({"results": page.iter().map(|r| r.1.clone()).collect::<Vec<_>>(), "since": date(since), "page": a.page, "total": total}),
                    );
                }
                if page.is_empty() {
                    return text(format!("Nothing changed since {}.", date(since)));
                }
                let mut out = format!("Changed since {} ({}):\n", date(since), pages_line(a.page, a.page_size, total));
                for r in page {
                    out.push_str(&format!("- {}\n", r.2));
                }
                text(out)
            }
        }
    }

    #[tool(
        description = "Note schemas (Picoschema). `command`: validate notes against their schema (one `note_type`, one note by `identifier`, or every type that has a schema); infer a schema from the notes of a `note_type`; diff a `note_type`'s notes against its schema to find drift. A schema is a note with type: schema whose `entity` field names the note type it describes and whose `schema` field maps each field to its type; a field ending in '?' is optional."
    )]
    async fn mem_schema(&self, Parameters(a): Parameters<SchemaArgs>) -> R {
        self.check_project(&a.project)?;
        let notes = self.store().all_notes()?;
        let schemas: BTreeMap<String, Map<String, Value>> = notes
            .iter()
            .filter(|n| n.note_type == "schema")
            .filter_map(|n| {
                Some((n.frontmatter.get("entity")?.as_str()?.to_lowercase(), n.frontmatter.get("schema")?.as_object()?.clone()))
            })
            .collect();
        let of_type = |t: &str| -> Vec<&NoteRow> { notes.iter().filter(|n| n.note_type.eq_ignore_ascii_case(t)).collect() };
        let field_name = |k: &str| k.trim_end_matches('?').to_string();
        let out: Value = match a.command {
            SchemaCommand::Infer => {
                let Some(t) = &a.note_type else { return bad_args("infer needs `note_type`") };
                let group = of_type(t);
                if group.is_empty() {
                    return fail(format!("No notes of type {t:?}"));
                }
                let mut counts: BTreeMap<String, (usize, String)> = BTreeMap::new();
                for n in &group {
                    for (k, v) in n.frontmatter.as_object().into_iter().flatten() {
                        if matches!(k.as_str(), "title" | "type" | "permalink") {
                            continue;
                        }
                        let kind = match v {
                            Value::Array(_) => "array",
                            Value::Number(_) => "number",
                            Value::Bool(_) => "boolean",
                            Value::Object(_) => "object",
                            _ => "string",
                        };
                        let e = counts.entry(k.clone()).or_insert((0, kind.into()));
                        e.0 += 1;
                    }
                }
                let total = group.len() as f64;
                let mut schema = Map::new();
                for (k, (c, kind)) in counts {
                    let share = c as f64 / total;
                    if share >= a.threshold {
                        let name = if share >= 0.999 { k } else { format!("{k}?") };
                        schema.insert(name, json!(kind));
                    }
                }
                json!({"note_type": t, "notes": group.len(), "schema": schema})
            }
            SchemaCommand::Validate | SchemaCommand::Diff => {
                let types: Vec<String> = match (&a.identifier, &a.note_type) {
                    (Some(id), _) => vec![self.store().require(id)?.note_type],
                    (None, Some(t)) => vec![t.clone()],
                    (None, None) if matches!(a.command, SchemaCommand::Validate) => schemas.keys().cloned().collect(),
                    _ => return bad_args("diff needs `note_type`"),
                };
                let mut reports = Vec::new();
                for t in types {
                    let Some(schema) = schemas.get(&t.to_lowercase()) else {
                        reports.push(json!({"note_type": t, "error": "no schema note for this type"}));
                        continue;
                    };
                    let required: Vec<String> = schema.keys().filter(|k| !k.ends_with('?')).cloned().collect();
                    let declared: HashSet<String> = schema.keys().map(|k| field_name(k)).collect();
                    let mut group = of_type(&t);
                    if let Some(id) = &a.identifier {
                        let one = self.store().require(id)?;
                        group.retain(|n| n.id == one.id);
                    }
                    let mut problems = Vec::new();
                    let mut extra: BTreeMap<String, usize> = BTreeMap::new();
                    for n in &group {
                        let fm = n.frontmatter.as_object().cloned().unwrap_or_default();
                        let missing: Vec<&String> = required.iter().filter(|k| fm.get(*k).is_none_or(Value::is_null)).collect();
                        if !missing.is_empty() {
                            problems.push(json!({"path": n.path, "missing": missing}));
                        }
                        for k in
                            fm.keys().filter(|k| !declared.contains(*k) && !matches!(k.as_str(), "title" | "type" | "permalink" | "tags"))
                        {
                            *extra.entry(k.clone()).or_default() += 1;
                        }
                    }
                    let mut r = json!({"note_type": t, "notes": group.len(), "invalid": problems});
                    if matches!(a.command, SchemaCommand::Diff) {
                        let unused: Vec<String> =
                            declared.iter().filter(|k| !group.iter().any(|n| n.frontmatter.get(k.as_str()).is_some())).cloned().collect();
                        r["undeclared_fields"] = json!(extra);
                        r["unused_schema_fields"] = json!(unused);
                    }
                    reports.push(r);
                }
                if reports.is_empty() {
                    return text("No schema notes in this project (a schema is a note with type: schema and an `entity` field).");
                }
                json!({"reports": reports})
            }
        };
        if a.output_format == Format::Json {
            return json_out(out);
        }
        text(serde_json::to_string_pretty(&out).unwrap_or_default())
    }

    #[tool(
        description = "Read this server's manual. With no arguments, the index of every page. `page` reads one tool's page ('mem_search' or 'mem-search(3)'). `query` lists the pages that mention every word given."
    )]
    async fn mem_man(&self, Parameters(a): Parameters<ManArgs>) -> R {
        let tools = self.tool_router.list_all();
        let page_of = |t: &Tool| -> String {
            let mut s = format!("{}(3)\n\n{}\n\nParameters:\n", t.name.replace('_', "-"), t.description.as_deref().unwrap_or(""));
            if let Some(Value::Object(props)) = t.input_schema.get("properties") {
                for (k, v) in props {
                    let d = v.get("description").and_then(Value::as_str).unwrap_or("");
                    s.push_str(&format!("  {k}: {d}\n"));
                }
            }
            s
        };
        if let Some(p) = &a.page {
            let want = p.trim().trim_end_matches("(3)").replace('-', "_");
            return match tools.iter().find(|t| t.name == want) {
                Some(t) => text(page_of(t)),
                None => fail(format!("No manual page {p:?}; mem_man with no arguments lists them")),
            };
        }
        let words: Vec<String> = a.query.as_deref().unwrap_or("").split_whitespace().map(search::fold).collect();
        let hits: Vec<&Tool> = tools.iter().filter(|t| words.iter().all(|w| search::fold(&page_of(t)).contains(w.as_str()))).collect();
        if a.output_format == Format::Json {
            return json_out(json!(
                hits.iter()
                    .map(|t| json!({"page": format!("{}(3)", t.name.replace('_', "-")), "summary": t.description}))
                    .collect::<Vec<_>>()
            ));
        }
        let mut out = String::from("Manual pages:\n");
        for t in hits {
            let summary = t.description.as_deref().unwrap_or("").split(". ").next().unwrap_or("").to_string();
            out.push_str(&format!("  {}(3) — {summary}\n", t.name.replace('_', "-")));
        }
        text(out)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("muninn-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "A Markdown knowledge base: notes with frontmatter, observations ('- [category] fact #tag') \
                 and relations ('- relation_type [[Other Note]]'). Search with mem_search (hybrid by default), read with mem_read, \
                 follow links with mem_context, and write with mem_write and mem_edit. This server holds one project, so `project` may be omitted.",
            )
    }

    /// The router's dispatch, plus one log line per call: what was used, how long it took, how much came back.
    /// Arguments are not logged, since they hold note content.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let mode = request.arguments.as_ref().and_then(|a| a.get("mode")).and_then(Value::as_str).map(str::to_owned);
        let started = std::time::Instant::now();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).await;
        let ms = started.elapsed().as_millis();
        let mode = mode.map(|m| format!(" mode={m}")).unwrap_or_default();
        match &result {
            Ok(CallToolResponse::Complete(r)) => {
                let out: String = r.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect();
                let status = if r.is_error == Some(true) {
                    "error"
                } else if out.starts_with("No results") || out.starts_with("Nothing changed") {
                    "empty"
                } else {
                    "ok"
                };
                log::info!("call {name}{mode} {status} {ms}ms {} chars", out.chars().count());
            }
            Err(e) => log::info!("call {name}{mode} invalid {ms}ms: {}", e.message),
            Ok(_) => log::info!("call {name}{mode} pending {ms}ms"),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside() {
        assert_eq!(clean_rel("main", "main/notas/x.md").unwrap(), "notas/x.md");
        assert_eq!(clean_rel("main", "/").unwrap(), "");
        assert!(clean_rel("main", "../etc/passwd").is_err());
        assert!(clean_rel("main", "a/.git/config").is_err());
        assert_eq!(with_md("a/v1.2 plano"), "a/v1.2 plano.md");
        assert_eq!(with_md("a/b.md"), "a/b.md");
    }

    #[test]
    fn read_only_hides_write_tools() {
        let (_d, store) = crate::store::tests::store_with(&[]);
        let store = Arc::new(store);
        let ctx = |read_only| {
            let vectors = Arc::new(crate::embed::Vectors::default());
            Server::new(Arc::new(Ctx { store: Arc::clone(&store), embedder: None, vectors, read_only, project_id: None, demoted: vec![] }))
        };
        // The handler's view, which is what clients list and call.
        assert!(ctx(true).get_tool("mem_write").is_none());
        assert!(ctx(true).get_tool("mem_read").is_some());
        assert!(ctx(false).get_tool("mem_write").is_some());
    }

    #[test]
    fn tool_reference_matches_the_code() {
        // docs/tools.md is generated from tools/list; this fails when a tool, a parameter or a description drifts.
        let doc = include_str!("../docs/tools.md");
        let flat = |s: &str| s.replace('|', "\\|").replace('\n', " ");
        for t in Server::tool_router().list_all() {
            let start = doc.find(&format!("\n## {}\n", t.name)).unwrap_or_else(|| panic!("docs/tools.md has no section for {}", t.name));
            let section = doc[start + 1..].split("\n## ").next().unwrap();
            assert!(section.contains(&flat(t.description.as_deref().unwrap_or(""))), "{}: description differs", t.name);
            let props = t.input_schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
            for (k, v) in props {
                let row =
                    section.lines().find(|l| l.starts_with(&format!("| `{k}` |"))).unwrap_or_else(|| panic!("{}: no row for {k}", t.name));
                let d = v.get("description").and_then(Value::as_str).unwrap_or("");
                assert!(row.contains(&flat(d)), "{}.{k}: description differs", t.name);
            }
        }
    }

    #[tokio::test]
    async fn a_query_of_tags_alone_filters_by_tag() {
        let (_d, store) = crate::store::tests::store_with(&[
            ("a.md", "---\ntags: [coffee]\n---\n# A\n\nbrewing\n"),
            ("b.md", "---\ntags: [tea]\n---\n# B\n\nbrewing\n"),
        ]);
        let vectors = Arc::new(crate::embed::Vectors::default());
        let s = Server::new(Arc::new(Ctx {
            store: Arc::new(store),
            embedder: None,
            vectors,
            read_only: false,
            project_id: None,
            demoted: vec![],
        }));
        let args = serde_json::from_value(json!({"project": "main", "query": "tag:coffee"})).unwrap();
        let out = s.mem_search(Parameters(args)).await.unwrap();
        let body = serde_json::to_string(&out.content).unwrap();
        assert!(body.contains("a.md") && !body.contains("b.md"), "{body}");
    }

    #[tokio::test]
    async fn project_is_optional_and_context_keeps_old_relations() {
        let (_d, store) = crate::store::tests::store_with(&[("a.md", "# A\n\n- relates_to [[B]]\n"), ("b.md", "# B\n\nold\n")]);
        let month_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400);
        std::fs::File::options().write(true).open(store.abs("b.md")).unwrap().set_modified(month_ago).unwrap();
        store.index_file("b.md").unwrap();
        let vectors = Arc::new(crate::embed::Vectors::default());
        let s = Server::new(Arc::new(Ctx {
            store: Arc::new(store),
            embedder: None,
            vectors,
            read_only: false,
            project_id: None,
            demoted: vec![],
        }));
        let context = |since: Value| {
            let s = s.clone();
            async move {
                let mut args = json!({"identifier": "a.md"});
                if !since.is_null() {
                    args["since"] = since;
                }
                let out = s.mem_context(Parameters(serde_json::from_value(args).unwrap())).await.unwrap();
                serde_json::to_string(&out.content).unwrap()
            }
        };
        assert!(context(Value::Null).await.contains("b.md"), "no limit by default");
        assert!(!context(json!("7d")).await.contains("(b.md"), "since still limits");
        let found = s.mem_search(Parameters(serde_json::from_value(json!({"query": "old"})).unwrap())).await.unwrap();
        assert!(serde_json::to_string(&found.content).unwrap().contains("b.md"));
    }

    #[test]
    fn repeats_must_agree_on_numbers() {
        assert!(same_numbers("Restart the gateway, then recreate the sidecar", "Recreate the sidecar after restarting the gateway"));
        assert!(same_numbers("Weight 117,0 kg at 07:50", "Weight 117,0 kg at 07:50 (manual entry)"));
        assert!(!same_numbers("Weight 117,0 kg at 07:50", "Weight 116,7 kg at 06:41"));
        assert!(!worth_comparing("Next steps:"));
    }

    #[test]
    fn symlinks_are_refused() {
        let (_d, store) = crate::store::tests::store_with(&[("a/n.md", "x\n")]);
        let out = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(out.path(), store.abs("fora")).unwrap();
        assert!(inside(&store, "fora/novo").is_err());
        assert_eq!(inside(&store, "a/novo/x").unwrap(), "a/novo/x");
    }
}
