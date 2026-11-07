# Using Muninn

This guide is for people whose assistant uses Muninn, and for the assistant itself: how to write notes that search well, how to find them again, and how to change them safely. Installing and running the server is in [Running Muninn](operations.md). Every parameter is in the [tool reference](tools.md).

## The basics

- **A note is a Markdown file** in the notes folder. You can write it in any editor, or have the assistant write it with `mem_write`. Edits made outside the server are picked up within about 15 seconds.
- **A project** is the notes folder this server holds. Tools take its name as `project` (by default `main`); since the server holds one project, `project` may be left out. `mem_project` with `command: "list"` shows it.
- **A note is found by** its path (`food/coffee-brewing.md`), its permalink (`main/food/coffee-brewing`), a `memory://` URL (`memory://main/food/coffee-brewing`), or its exact title (`Coffee brewing`). Any of these works wherever a tool asks for `identifier`.

## Writing notes that search well

A note can be plain prose and still be found. Three habits make it much easier to find, and let the assistant build on it:

```markdown
---
title: Coffee brewing
type: note
tags: [coffee]
---

# Coffee brewing

## Method
- [method] Pour-over gives a cleaner cup than the French press #brewing
- [ratio] 1:16 coffee to water, by weight (adjust for the roast)

## Decisions
- [decision] Buy a burr grinder before a better kettle #equipment

## Related
- relates_to [[Coffee beans]]
- inspired_by [[James Hoffmann]]
```

**One subject per note, with a title that names it.** Title search and `mem_read` by title only work when the title is what you would ask for. Split a note that covers two subjects.

**Headings.** Search reads a note passage by passage, one passage per heading section. A note with headings is matched by the section that answers the question, and `mem_read` can return that section alone.

**Observations** are list items that start with a category in brackets: `- [decision] ...`, `- [fact] ...`, `- [idea] ...`. Choose the categories you like and reuse them. Each observation is indexed on its own, so it can be searched by category (`categories: ["decision"]`) across every note. `#tags` inside it and a trailing `(context)` are kept apart.

**Relations** are list items of the form `- relation_type [[Other note]]`. They make the notes a graph that `mem_context` can walk. A `[[link]]` anywhere else in the text counts too, as `links_to`. A link to a note that does not exist yet is fine: it resolves once that note is written.

**Frontmatter** fields beyond `title`, `type` and `tags` are kept and can be searched: `status: active`, `client: Acme`, `due: 2026-12-01`. Use `type` for kinds of note you keep many of (`person`, `meeting`, `decision`), so searches can be limited to them.

The full rules for what counts as an observation or a relation are in [Architecture](architecture.md#the-note-format).

## Finding things

**Ask in plain words.** With an embedding server configured, `mem_search` defaults to hybrid search: it finds notes by meaning, so "how strong should the coffee be?" finds the note that says "1:16 coffee to water".

```json
{"project": "main", "query": "how strong should the coffee be?"}
```

**Use exact words for exact things.** Names, error messages and identifiers are found best by text search. Quote a phrase to require it as written; combine terms with `AND`, `OR`, `NOT`; match a tag with `tag:coffee` (alone, it lists every note with that tag). Accents don't matter: `cafe` finds `café`.

```json
{"project": "main", "query": "\"burr grinder\" NOT espresso", "mode": "text"}
```

**Superseded facts rank lower** when the server lists their categories in `MUNINN_DEMOTE_CATEGORIES` (for example `superseded,closed`). They are still found; they come after a current fact that matches as well.

**Narrow before you search.** These work in every mode and can be combined:

| To limit to | Use |
|---|---|
| a folder | `"path": "food"` |
| a kind of note | `"note_types": ["meeting"]` |
| a frontmatter value | `"filters": {"status": "active"}`, or `{"due": {"$lt": "2026-12-01"}}` |
| recent changes | `"since": "7d"`, `"last week"`, or a date |
| decisions, facts, ideas... | `"items": ["observation"], "categories": ["decision"]` |

**Search without a question.** Leave out `query` to list what the filters match: every active project, every decision this week.

```json
{"project": "main", "items": ["observation"], "categories": ["decision"], "since": "7d"}
```

**Find the lines, not just the note.** `lines` returns the matching lines of each note with that many lines around them, like grep. `identifier` searches inside one note.

```json
{"project": "main", "query": "ratio", "identifier": "Coffee brewing", "lines": 1}
```

**Other ways in.**

- `mem_find` finds files by name (`"name": "*meeting*"`) or by frontmatter. With `fields`, it returns only the chosen fields of each note, which makes a quick table.
- `mem_list` browses one folder.
- `mem_project` with `command: "recent"` shows what changed lately.
- `mode: "title"` and `mode: "permalink"` match those fields directly, with `*` globs.

## Reading

`mem_read` returns a whole note by default. A long note doesn't have to be read whole:

- `"section": "Decisions"` returns one section. Nested headings are written `"Auth/Decisions"`, and the second section of the same name is `"Decisions[1]"`.
- `start_line` and `end_line` return a range. Line numbers count the whole file, frontmatter included, the same as the `lines` search results.
- `max_tokens` cuts the note to a budget, at a section or paragraph boundary.

`mem_context` returns a note together with the notes it relates to, in both directions, up to five hops away. Use it to collect the background of a subject before working on it. `compact: true` returns only the shape of the graph, to pick which notes to read. `since: "30d"` keeps only the related notes changed in that time.

## Changing notes

**Create** with `mem_write`. Give the file path as `identifier`, or a `title` and a `directory`. The title comes from the frontmatter, then `title`, then the file name, never from the `#` heading, so pass `title` when the file name is not the title. Writing over an existing note is refused unless you pass `overwrite: true`.

```json
{"project": "main", "identifier": "food/coffee-beans.md", "title": "Coffee beans", "content": "# Coffee beans\n\n- [fact] Light roasts suit pour-over #roast\n- used_in [[Coffee brewing]]"}
```

**Change part of a note** with `mem_edit`, which leaves the rest of the file untouched:

| `command` | Does |
|---|---|
| `append`, `prepend` | Add `content` at the end, or at the start (after the frontmatter). |
| `find_replace` | Replace the exact text `find_text` with `content`. It must occur `expected_replacements` times (default 1), or nothing changes. |
| `replace_section` | Replace the body of the heading `section`. |
| `insert_before_section`, `insert_after_section` | Add `content` next to that heading. |
| `set_metadata` | Merge `metadata` into the frontmatter, leaving the body alone. |

```json
{"project": "main", "identifier": "Coffee brewing", "command": "append",
 "content": "- [decision] Grind finer for light roasts #brewing"}
```

**Repeats are pointed out.** With an embedding server, a write that adds observations also compares them with the ones already saved. When one says nearly the same thing, the result lists it, with its note and line:

```text
Already saved, nearly the same (update one of these rather than keeping both):
- [pitfall] Restarting the gateway drops the sidecar's network; recreate it... (infra/gateway.md:15, similarity 0.87)
```

The check is best effort: when the embedding server is busy, the result comes back without it. The write is made either way. Keep one copy: delete the new line, or move the fact to the note it belongs in. Observations that differ only in their numbers (two days' readings) are not counted as repeats.

**Move, rename and delete** with `mem_move` and `mem_delete`, for one note (`identifier`) or a folder (`path`). Links by title keep working after a move. Deleting a folder needs `recursive: true`. Nothing deleted can be recovered by the server: keep the notes folder under git or in a backup.

**Don't overwrite someone else's edit.** Every read returns a checksum. Pass it as `expected_checksum` to `mem_write`, `mem_edit`, `mem_move` or `mem_delete`, and the change is made only if the note is still the version you read. If you or another assistant changed it in between, the call fails and the note stays as it was: read it again and redo the change.

## Errors

A call that cannot be done comes back as a result with the reason, so the assistant can correct it and try again:

- *not found*: the note doesn't exist. Find it with `mem_search` first.
- *already exists*: `mem_write` on an existing note. Pass `overwrite: true`, or use `mem_edit`.
- *checksum*: the note changed since it was read. Read it again.
- *find_text occurs N times*: make `find_text` longer, or set `expected_replacements`.
- *unknown project*: use the name `mem_project` lists.

A path that leaves the notes folder (`..`, a hidden folder such as `.git`, a symlink) is refused outright.

## Guiding the assistant

Clients pass the tool descriptions to the model, so it already knows the tools. A few lines in your assistant's instructions (`CLAUDE.md`, a system prompt, a project prompt) make it use them well:

```markdown
## Memory

Long-term memory is the Muninn MCP server, project `main`.
- Before assuming context about a project, person or preference, search it with mem_search.
- When work produces something worth keeping (a decision, a fact, a fix), save it:
  mem_edit an existing note on the subject, or mem_write a new one if there is none.
- One subject per note. Record facts as observations ("- [decision] ...") and link related
  notes with relations ("- relates_to [[Other note]]").
- Read before changing a note, and pass its checksum as expected_checksum.
- Never delete a note unless asked.
```

`mem_man` serves the manual from the server, so an assistant that is unsure can read it without these docs.
