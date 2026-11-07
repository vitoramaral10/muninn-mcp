# Architecture

Muninn is one process with three parts: a folder of Markdown notes, an SQLite index built from them, and an MCP server over HTTP. The notes are the only source of truth. Everything in the index can be rebuilt from the folder, and the one thing a rebuild costs is time: the embedding cache has to be filled again.

```
                     ┌──────────────── muninn-mcp ────────────────┐
 MCP client ──HTTP──▶│ /mcp   tools (src/mcp.rs)                  │
                     │          │ read            │ write          │
                     │          ▼                 ▼                │
                     │   search (search.rs)   vault (vault.rs) ───────▶ notes/*.md
                     │          │                 │ reindex file   │        ▲
                     │          ▼                 ▼                │        │
                     │   index: muninn.db (store.rs) ◀── scan every 15 s ───┘
                     │          ▲                                  │
                     │          │ vectors                          │
                     │   embed backfill (embed.rs) ──HTTP──────────────▶ /v1/embeddings
                     └─────────────────────────────────────────────┘
```

| Module | Holds |
|---|---|
| `note.rs` | Parsing one note: frontmatter, headings, observations, relations, slugs and permalinks. |
| `store.rs` | The SQLite schema, the folder scan, indexing a file, chunking, and every query the tools need. |
| `embed.rs` | The client for the embedding server, the background backfill, and the in-memory vector set. |
| `search.rs` | Query parsing, filters, the text and meaning searches, and their fusion. |
| `vault.rs` | Changing note text: composing frontmatter, the `mem_edit` commands, slicing a read, atomic writes. |
| `mcp.rs` | The twelve tools, argument checking, path safety and read-only mode. |
| `main.rs` | Configuration from the environment, the HTTP server, and the `index`, `healthcheck` and `version` commands. |

## The note format

A note is a Markdown file with YAML frontmatter; observations and relations are plain list items.

```markdown
---
title: Coffee brewing
type: note
permalink: main/food/coffee-brewing
tags:
- coffee
---

# Coffee brewing

- [method] Pour-over gives a cleaner cup than the French press #brewing
- [ratio] 1:16 coffee to water (by weight)
- relates_to [[Coffee beans]]
- inspired_by [[James Hoffmann]] (his V60 video)

See also [[Grinders]].
```

**Frontmatter.** `title` falls back to the file name, `type` to `note`, and `permalink` to `<project>/<slug of the path>`. `tags` takes a list or a comma-separated string. Every other field is kept and can be searched with `filters`.

**Observations** are list items that start with `[category]`:

- the text after the brackets is the content;
- `#hashtags` in it become the observation's tags;
- a trailing parenthesized group becomes its context;
- a list item made only of hashtags is an observation without a category.

These are not observations: task items (`[ ]`, `[x]`, `[-]`), a category that looks like a timestamp (`[10:30]`), and a Markdown link (`[text](url)`).

**Relations** are list items of the form `relation_type [[Target]]`:

- the relation type is one word, or several in quotes;
- after the target there may be nothing, or one parenthesized context;
- a line with any other text after the link is not an explicit relation.

Every other `[[link]]` in the note counts as a `links_to` relation.

A target resolves to a note by path, permalink, then title (ignoring case), then slug. One that matches nothing stays unresolved and resolves later, when a note by that name appears.

**What is skipped.** List items may be bulleted (`-`, `*`, `+`) or numbered (`1.`, `1)`). Indented continuation lines join the item. Text inside fenced code blocks is never read as an observation or relation.

## The index

`muninn.db` lives in `MUNINN_DATA_DIR` and holds:

| Table | Contents |
|---|---|
| `notes` | One row per Markdown file: path, permalink, title, type, tags, frontmatter as JSON, checksum, size and mtime. |
| `observations`, `relations` | Parsed from each note, with their line numbers. Relations keep both the written target and the note it resolves to. |
| `notes_fts`, `obs_fts`, `chunks_fts` | FTS5 tables with `unicode61 remove_diacritics 2`, so `cafe` finds `café`. |
| `chunks` | The passages that get embedded (see below), with the SHA-256 of their text. |
| `vectors` | Embeddings keyed by (text hash, model). |

**Chunks.** Each note is split into passages for embedding and passage-level text search:

- one per heading section, prefixed with the note's title, cut at 900 characters with 120 of overlap;
- one per observation: `title`, then `category: content`;
- one per relation: `title -> target`, then the relation type and its context.

**The vector cache.** Vectors are keyed by the hash of the chunk's text and the model's name, so:

- an edit re-embeds only the passages whose text changed;
- renaming or moving a note re-embeds nothing;
- switching `EMBED_MODEL` starts a separate cache and leaves the old one in place.

Vectors that no chunk uses any more are pruned.

**Parser versions.** When a release changes how notes are parsed, it bumps `PARSER_VERSION`. On the next start, every note is read again. Vectors are reused wherever the passage text is unchanged.

## Keeping the index current

**At start, and then every `MUNINN_SCAN_SECONDS`, the folder is compared with the index:**

- a file whose size and mtime match its row is skipped;
- otherwise its SHA-256 is compared, and only a changed file is parsed again;
- a file that disappeared is dropped from the index.

So an edit made by an editor, `git pull` or a sync tool reaches search within one scan. Hidden files and folders, and symlinks, are never read.

**A write through a tool indexes the file it changed straight away.** A search right after `mem_write` already sees the note's text.

**Embedding runs apart from writes.** A background loop sends every chunk without a vector to the embedding server, `EMBED_BATCH` chunks per request. Until a passage has its vector, it can be found by text and not yet by meaning. Search reads vectors from a copy kept in memory, which is rebuilt after new vectors arrive.

## Search

`mem_search` has five modes:

| Mode | How it ranks |
|---|---|
| `text` | FTS5 with BM25 over title, body, permalink and tags. All terms are required first; if that finds nothing, any term will do. Words of three letters or more also match as prefixes. |
| `semantic` | Cosine similarity between the query's embedding and every chunk. A note scores its best chunk. Chunks under `min_similarity` (default 0.35) are dropped. |
| `hybrid` | Text and semantic fused by reciprocal rank (k = 60, the top 60 of each list). |
| `title`, `permalink` | Direct match, with `*` globs. |

**Hybrid's text side differs from `text` mode** in two ways:

- **It matches inside one passage.** Every term must appear in the same chunk, not just somewhere in the note. Otherwise a long note matches any question through its common words, and that noise outranks the notes that match by meaning.
- **It never falls back to "any term".** When no passage holds every term, the text side is left out and the ranking is by meaning alone. This is the case for most questions asked in plain language.

Scores are scaled so that a result ranked first in both lists scores 1.0.

**Demoted categories.** Observations in a category listed in `MUNINN_DEMOTE_CATEGORIES` keep 95% of their score, in meaning search and when observations are searched by text. A superseded fact then ranks after a current one that matches as well, and still comes first when nothing else matches.

The same machinery searches observations (`items: ["observation"]`) and relations. Filters by folder, note type, tags, frontmatter (`filters`) and date (`since`) are applied before ranking, the same way in every mode.

## Writes

**Every write is atomic.** The new text goes to a temporary file in the same folder, which is then renamed over the note. A reader, `git` or a sync tool never sees half a file. Each write's temporary file has a unique name, so two writes to one note cannot interfere.

**Concurrent edits are guarded by checksums.** `mem_read` returns the note's checksum. `mem_write`, `mem_edit`, `mem_move` and `mem_delete` accept it as `expected_checksum` and refuse to act if the file has changed since. Without it, the last write wins.

**`mem_write` composes the frontmatter in a fixed order:** `title`, `type`, `permalink` and `tags` first, then any other fields given. A new note's permalink comes from its path.

**Repeats are pointed out.** With embeddings, `mem_write` and `mem_edit` compare each observation a write added (up to five, of 30 characters or more) with the ones already saved, in two steps:

1. The stored vectors shortlist up to three candidates each (cosine 0.72 or more). Those vectors include the note title, which pulls repeats in different notes apart, so they only shortlist.
2. The added observation and each candidate are compared on their own text, without the title. 0.85 or more is a repeat, unless the two hold different numbers.

On a real base, known repeats scored 0.86 to 0.97 on the second step and related facts up to 0.84. The vectors of the text without the title are cached under `<model>:bare`. The check costs one or a few embedding calls, about a second on a CPU when the embedding server is idle. It gets at most 3 seconds and never blocks the write: when the server is busy or fails, the result has no list.

**`mem_move` keeps the permalink**, so links by permalink survive a move, and so do links by title.

**SQLite keeps temporary data in memory** (`temp_store=MEMORY`), so it works on a read-only root filesystem.

## Safety

- **Paths stay in the notes folder.** A path from a client may not contain `..`, a hidden segment (`.git`, `.obsidian`), a backslash or a NUL byte, and may not pass through a symlink. Any of these is refused before the filesystem is touched.
- **Read-only mode** (`MUNINN_READ_ONLY=1`) removes the four write tools from the server, so they are neither listed nor callable. Mounting the notes read-only as well makes it a second line of defense.
- **The HTTP endpoint has no authentication**. Keep it on a private network or behind a gateway that authenticates clients. See [SECURITY.md](https://github.com/vitoramaral10/muninn-mcp/blob/main/SECURITY.md).
