# Tool reference

Every tool Muninn serves, with its parameters. The descriptions here are the same ones clients receive in `tools/list`; `mem_man` serves them from the server too.

Muninn serves one project per server, so `project` may be omitted. The project is named by `MUNINN_PROJECT` (default `main`); `MUNINN_PROJECT_ID` adds a second accepted name. A tool given another project returns an error.

With `MUNINN_READ_ONLY=1` the write tools (`mem_write`, `mem_edit`, `mem_move`, `mem_delete`) are not listed, and calling one returns *tool not found*.

| Tool | Writes | Summary |
|---|---|---|
| [`mem_search`](#mem_search) |  | Search the knowledge base, ranked by relevance. |
| [`mem_read`](#mem_read) |  | Read a note or file by path, permalink, memory:// URL or exact title. |
| [`mem_write`](#mem_write) | yes | Create or replace a note. |
| [`mem_edit`](#mem_edit) | yes | Change part of an existing note. |
| [`mem_move`](#mem_move) | yes | Move or rename a note (`identifier`) or a directory (`path`) within the project. |
| [`mem_delete`](#mem_delete) | yes | Delete a note (`identifier`) or a directory (`path`). |
| [`mem_list`](#mem_list) |  | List one folder of the project: its subfolders, then its files with title, date and permalink. |
| [`mem_find`](#mem_find) |  | Find files in the project, recursively. |
| [`mem_context`](#mem_context) |  | Follow the knowledge graph from a note: the note, its observations, and the notes its relations reach (both directions), up to `depth` hops (1-5). |
| [`mem_project`](#mem_project) |  | Projects. |
| [`mem_schema`](#mem_schema) |  | Note schemas (Picoschema). |
| [`mem_man`](#mem_man) |  | Read this server's manual. |

## Results and errors

Tools answer in readable text by default. With `output_format: "json"` they answer with one JSON document instead, which suits programs and long result lists.

A call that cannot be done (an unknown note or project, a checksum that no longer matches, a note that already exists) comes back as a tool result marked `isError`, with the reason as text, so the model can read it and try again. A request that is wrong in itself is a protocol error (`invalid_params`): a missing required parameter, a value of the wrong type, or a path that leaves the notes folder (`..`, a hidden segment, a symlink).

## mem_search

Search the knowledge base, ranked by relevance. `query` supports quoted phrases, AND/OR/NOT and tag:name. `mode` picks hybrid (meaning and words fused; the default when embeddings are configured), text, semantic, title or permalink (globs with *). Narrow with `filters` (frontmatter), `note_types`, `items` (note, observation, relation), `categories` and `since`. `lines` returns each hit's matching lines with context, like grep. `path` limits the search to a folder; `identifier` searches one note. Read full notes with mem_read.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `query` | string |  | What to search for. Omit to search by filters alone. |
| `identifier` | string |  | Search only this note; returns its matching lines. |
| `all_projects` | boolean | `false` | Search every project. |
| `categories` | list of string |  | Observation categories to match exactly, e.g. ['decision']. |
| `compact` | boolean | `false` | Omit excerpts; keep identifiers and scores. |
| `filters` | object |  | Frontmatter filters as {field: condition}: a value for equality, null for absent-or-null, a list for contains-all, or an operator object with $gt, $gte, $lt, $lte, $in, $between, $contains or $exists. Nested fields use dot notation. |
| `items` | list of string |  | What to return: note, observation, relation, or a mix. Default: note. |
| `lines` | integer |  | Return each hit's matching lines with this many lines of context (0-10), instead of an excerpt. |
| `max_matches` | integer | `10` | Matching lines shown per note with `lines`. |
| `min_similarity` | number |  | Override the semantic similarity floor (0-1). |
| `mode` | string |  | Search mode: hybrid (default when semantic search is available), text, semantic, title or permalink. |
| `note_types` | list of string |  | Frontmatter `type` values, e.g. ['person', 'meeting']. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `page` | integer | `1` | Page number, from 1. |
| `page_size` | integer | `10` | Results per page (1-100). |
| `path` | string |  | Search only this folder and its subfolders. |
| `project` | string |  | Project name. Optional: this server holds one project, used when this is omitted. |
| `projects` | list of string |  | Search these projects together. |
| `since` | string |  | How far back to look: '7d', '24h', '2 days ago', 'last week', or a date. |
| `time_kind` | string |  | Not supported here: temporal qualifiers are not indexed. |
| `valid_at` | string |  | Not supported here. |
| `valid_overlaps` | string |  | Not supported here. |

## mem_read

Read a note or file by path, permalink, memory:// URL or exact title. Slice a long note with `section` (a heading), `start_line`/`end_line` or `max_tokens`; line numbers count the whole file, frontmatter included. `include_content=false` returns only the metadata and the checksum that mem_write, mem_edit, mem_move and mem_delete take as expected_checksum. Images come back as images. A missing note is an error: find notes with mem_search.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `identifier` | string | required | The note or file: a path ('notes/plan.md'), permalink, memory:// URL, exact title, or '<project>/path'. |
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `end_line` | integer |  | Last line to return, inclusive. |
| `include_content` | boolean | `true` | False returns only title, paths, checksum and updated_at. |
| `include_frontmatter` | boolean | `true` | Include the YAML frontmatter in a whole-note read. |
| `max_tokens` | integer |  | Approximate token budget; longer content is cut at a section or paragraph boundary. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `section` | string |  | A heading to read: "Decisions", "Auth/Decisions", or "Decisions[1]" for the second one. |
| `start_line` | integer |  | First line to return, counting the whole file (frontmatter included) from 1. |

## mem_write

Create or replace a note. Give a file path as `identifier` ('notes/plan.md'), or `title` plus `directory`. The title comes from the content's frontmatter, then `title`, then the file name. An existing note is an error unless `overwrite=true`; pass `expected_checksum` (from mem_read) to replace only the revision you read. To change part of a note use mem_edit. Content may hold frontmatter, observations ('- [category] fact #tag') and relations ('- relation_type [[Other Note]]'). The result lists observations already saved that say nearly what this write added: update one of those rather than keep both.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `content` | string | required | The note's Markdown, saved as given; it may start with frontmatter. |
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `identifier` | string |  | File path for the note, e.g. 'notes/plan.md'; '.md' is added when missing. Use instead of title + directory. |
| `directory` | string |  | Folder for a note addressed by title; '/' is the root. |
| `expected_checksum` | string |  | Replace the note only while it is still this revision (the checksum from mem_read). Implies overwrite. |
| `metadata` | object |  | Extra frontmatter fields, merged into the note's frontmatter. |
| `note_type` | string |  | Frontmatter `type:`. A `type:` in the content wins. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `overwrite` | boolean |  | Replace a note that already exists. |
| `tags` | any |  | Tags, as a list or 'a,b,c'. |
| `title` | string |  | Note title. With `directory`, it also names the file. |

## mem_edit

Change part of an existing note. `command`: append or prepend `content`; find_replace `find_text` with `content` (exact text that must occur `expected_replacements` times, default 1); replace_section, insert_before_section or insert_after_section for a `section` heading ('Decisions', 'Auth/Decisions', 'Decisions[1]'); set_metadata to merge `metadata` into the frontmatter without touching the body. `metadata` also merges with any other command. Pass `expected_checksum` (from mem_read) to edit only the revision you read. Like mem_write, the result lists observations already saved that say nearly what the edit added. Create notes with mem_write.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `identifier` | string | required | The note: a path, permalink, memory:// URL or exact title. |
| `command` | `append` \| `prepend` \| `find_replace` \| `replace_section` \| `insert_before_section` \| `insert_after_section` \| `set_metadata` | required | The edit: append, prepend, find_replace, replace_section, insert_before_section, insert_after_section or set_metadata. |
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `content` | string |  | The text to add or the replacement. Not used by set_metadata. |
| `expected_checksum` | string |  | Edit only while the note is still this revision (from mem_read). |
| `expected_replacements` | integer | `1` | For find_replace: how many times find_text must occur. |
| `find_text` | string |  | For find_replace: the exact, case-sensitive text to replace. |
| `metadata` | object |  | Frontmatter fields to set; other fields are kept. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `replace_subsections` | boolean | `true` | For replace_section: true replaces the subsections too; false stops at the first subsection heading. |
| `section` | string |  | The heading for section commands: "Decisions", "Auth/Decisions", or "Decisions[1]". |

## mem_move

Move or rename a note (`identifier`) or a directory (`path`) within the project. `destination` is the new project-relative path; ending it with '/' moves into that folder under the current name. Pass `expected_checksum` (from mem_read) to move a note only if it is still the revision you read. Links by title keep resolving; the permalink in the frontmatter does not change.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `destination` | string | required | New path, e.g. 'archive/plan.md'. A trailing '/' moves into that folder under the current name. |
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `identifier` | string |  | The note to move. |
| `expected_checksum` | string |  | Move the note only while it is still this revision (from mem_read). |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `path` | string |  | The directory to move, with everything in it. |

## mem_delete

Delete a note (`identifier`) or a directory (`path`). A directory delete removes every file under it and needs `recursive=true`. Pass `expected_checksum` (from mem_read) to delete a note only if it is still the revision you read. A note that does not exist is an error. This cannot be undone.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `identifier` | string |  | The note to delete. |
| `expected_checksum` | string |  | Delete the note only while it is still this revision (from mem_read). |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `path` | string |  | The directory to delete, with everything in it. |
| `recursive` | boolean | `false` | Required true to delete a directory. |

## mem_list

List one folder of the project: its subfolders, then its files with title, date and permalink. '/' lists the root. Filter files with `glob` and order them with `sort`. For a recursive search by name or frontmatter, use mem_find.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `glob` | string |  | File-name glob, e.g. '*.md' or '*meeting*'. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `page` | integer | `1` | Page number, from 1. |
| `page_size` | integer | `10` | Results per page. |
| `path` | string | `"/"` | A directory in the project; '/' is the root. |
| `sort` | `title_asc` \| `title_desc` \| `updated_asc` \| `updated_desc` |  | File order. Folders always come first. |

## mem_find

Find files in the project, recursively. With `name`, match file names against a glob ('*meeting*'). With `filters`, match notes by frontmatter, e.g. {"status": "active"} or {"priority": {"$in": ["high"]}}, and with `fields` return just those frontmatter values per note. `path` limits the search to a folder. For ranked text or meaning search, use mem_search.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `depth` | integer | `10` | Folder levels to search (1-10). |
| `fields` | list of string |  | Frontmatter fields to return per note (dot paths allowed). Needs `filters`. |
| `filters` | object |  | Frontmatter filters, as in mem_search. |
| `name` | string |  | File-name glob, e.g. '*meeting*'. Not with `filters`. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `page` | integer | `1` | Page number, from 1. |
| `page_size` | integer | `10` | Results per page. |
| `path` | string | `"/"` | A directory to search in; '/' is the root. |

## mem_context

Follow the knowledge graph from a note: the note, its observations, and the notes its relations reach (both directions), up to `depth` hops (1-5). `identifier` is a path, permalink, memory:// URL, or a pattern such as 'specs/*'. `since` keeps only related notes changed recently. `compact` returns the graph without note bodies; read chosen notes with mem_read.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `identifier` | string | required | Where to start: a note path, permalink, memory:// URL, or a pattern such as 'specs/*'. |
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `compact` | boolean | `false` | Omit note bodies and observations; keep the graph shape. |
| `depth` | integer | `1` | Relation hops to follow (1-5). |
| `max_related` | integer | `10` | Most related results to return (0-100). |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `page` | integer | `1` | Page number, from 1. |
| `page_size` | integer | `10` | Primary results per page (1-50). |
| `since` | string |  | Only related notes changed this recently: '7d', '24h', 'last week', or a date. Default: no limit. |

## mem_project

Projects. `command`: list (the project this server holds); recent (what changed lately in `project`: `since`, `items`, paging); create is not supported, this server holds one project.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `command` | `list` \| `create` \| `recent` | required | list shows the project; recent shows what changed lately; create is refused (one project per server). |
| `items` | list of string |  | For recent: notes, observations, relations, or a mix. Default: notes. |
| `name` | string |  | For create: not supported, this server holds one project. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `page` | integer | `1` | Page number, from 1. |
| `page_size` | integer | `10` | Results per page. |
| `path` | string |  | For create: accepted for compatibility and ignored. |
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `set_default` | boolean | `false` | For create: accepted for compatibility and ignored. |
| `since` | string | `"7d"` | For recent: how far back to look. |
| `workspace` | string |  | Accepted for compatibility and ignored: there are no workspaces. |

## mem_schema

Note schemas (Picoschema). `command`: validate notes against their schema (one `note_type`, one note by `identifier`, or every type that has a schema); infer a schema from the notes of a `note_type`; diff a `note_type`'s notes against its schema to find drift. A schema is a note with type: schema whose `entity` field names the note type it describes and whose `schema` field maps each field to its type; a field ending in '?' is optional.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `command` | `validate` \| `infer` \| `diff` | required | validate checks notes against their schema; infer suggests one from existing notes; diff compares the two. |
| `project` | string |  | The project name. Optional: this server holds one project, used when this is omitted. |
| `identifier` | string |  | For validate: one note, instead of a whole note type. |
| `note_type` | string |  | The frontmatter `type:` to check. Required for infer and diff. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `threshold` | number | `0.25` | For infer: the share of notes a field must appear in to be suggested. |

## mem_man

Read this server's manual. With no arguments, the index of every page. `page` reads one tool's page ('mem_search' or 'mem-search(3)'). `query` lists the pages that mention every word given.

| Parameter | Type | Default | Description |
|---|---|---|---|
| `query` | string |  | Words to find in the manual; lists the pages that mention them all. |
| `output_format` | `text` \| `json` | `"text"` | text (readable, the default) or json (structured, for programs). |
| `page` | string |  | The page to read: a tool name such as 'mem_search' or 'mem-search(3)'. |
