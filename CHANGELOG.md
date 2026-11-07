# Changelog

Notable changes, newest first. The format follows [Keep a Changelog](https://keepachangelog.com),
and versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added
- Documentation: a tool reference generated from the tool schemas, an architecture guide, a guide to
  running Muninn, and contributing and security policies.
- A description for every tool parameter, including `project`, `page` and `output_format`.
- A test that fails when the tool reference and the code disagree.
- A guide to using the server: writing notes that search well, finding, reading and changing them.
- `mem_write` and `mem_edit` list the observations already saved that say nearly what the write added.
- `MUNINN_DEMOTE_CATEGORIES`: observation categories (such as `superseded`) that rank after current facts.
- One log line per tool call: the tool, the outcome, the time and the size of the answer.

### Changed
- `project` is optional in every tool, since the server holds one project.
- `mem_context` returns every related note by default; `since` still limits them.

### Fixed
- Writing a large note failed with *disk I/O error* (after the file was saved) when the container's root was read-only: SQLite now keeps temporary data in memory.
- A search for tags alone (`tag:coffee`) found nothing; it now lists the notes with those tags.

## [0.1.0] - 2026-11-05

### Added
- Twelve `mem_*` tools over a folder of Markdown notes: search, read, write,
  edit, move, delete, list, find, context, project, schema and the manual.
- An SQLite index with FTS5 (accent-insensitive) over notes, observations and passages.
- Embeddings from any OpenAI-compatible `/v1/embeddings` server, cached by passage text, so an edit
  re-embeds only what changed and a rename re-embeds nothing.
- Hybrid search by reciprocal rank fusion. Its text side matches inside one passage, and falls back
  to meaning alone when no passage holds every term.
- A periodic scan that indexes edits made outside the server.
- Read-only mode, which hides the write tools.
- `muninn-mcp index`, to build the index on one machine and copy it to another.
- A 7 MB static image on `scratch` that runs as an unprivileged user.

### Security
- Paths from clients cannot leave the notes folder through `..`, hidden segments or symlinks.
- Atomic writes, each with its own temporary file.

[Unreleased]: https://github.com/vitoramaral10/muninn-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/vitoramaral10/muninn-mcp/releases/tag/v0.1.0
