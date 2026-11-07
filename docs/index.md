---
layout: home
title: Muninn
titleTemplate: Long-term memory for AI assistants

hero:
  name: Muninn
  text: Long-term memory for AI assistants
  tagline: An MCP server in one static Rust binary. Hybrid search, a knowledge graph and safe writes over a folder of Markdown notes you can open in any editor.
  image:
    src: /logo.svg
    alt: The letter M drawn as a graph of linked notes
  actions:
    - theme: brand
      text: Get started
      link: /getting-started
    - theme: alt
      text: Using Muninn
      link: /usage
    - theme: alt
      text: View on GitHub
      link: https://github.com/vitoramaral10/muninn-mcp

features:
  - icon: 📝
    title: Your notes stay yours
    details: Each note is a Markdown file with YAML frontmatter, readable in any editor and versionable with git. The index beside them is derived; delete it and it is rebuilt.
  - icon: 🔎
    title: Search that understands the question
    details: Full-text BM25, accent-insensitive, fused with embeddings from any OpenAI-compatible server. Exact names and vague questions both land.
    link: /architecture#search
    linkText: How search ranks
  - icon: 🕸️
    title: A knowledge graph from plain lists
    details: "<code>- [decision] Use Postgres</code> is an observation, <code>- depends_on [[Auth]]</code> is a relation. mem_context walks the graph both ways."
    link: /architecture#the-note-format
    linkText: The note format
  - icon: 🪶
    title: Small and quick
    details: A 7 MB image on scratch that runs in about 50 MiB. On a 170-note base, hybrid search answers in 0.6 s.
  - icon: 🔄
    title: Edits from anywhere
    details: A scan every 15 seconds picks up changes from an editor, git pull or a sync tool. Only what changed is embedded again.
  - icon: 🛡️
    title: Safe by default
    details: Atomic writes guarded by checksums, paths locked to the notes folder, a read-only mode, and a warning when a write repeats a saved fact.
    link: /architecture#safety
    linkText: Safety
---

<div class="home-section">

## Ask in plain words, get your own notes back

<p class="lead">Your assistant forgets everything when the conversation ends. Muninn gives it somewhere to write things down, and a way to find them again by meaning, not only by the words you happened to use.</p>

<div class="home-grid">
<div>

### The conversation

```text
you   ▸ How strong should the coffee be?
model ▸ mem_search
        { "query": "how strong should
          the coffee be?" }
      → food/coffee-brewing.md §Method
        [ratio] 1:16 coffee to water
model ▸ Your note says 1:16 by
        weight, adjusted for the roast.
```

</div>
<div>

### The note it found

```markdown
---
title: Coffee brewing
tags: [coffee]
---

# Coffee brewing

## Method
- [method] Pour-over: a cleaner cup #brewing
- [ratio] 1:16 coffee to water

## Related
- relates_to [[Coffee beans]]
```

</div>
</div>

</div>

<div class="home-section">

## Running in a minute

<p class="lead">One container, one folder of notes. Add an embedding server later for search by meaning.</p>

```sh
docker build -t muninn-mcp https://github.com/vitoramaral10/muninn-mcp.git
docker run -d --name muninn -p 127.0.0.1:8000:8000 \
  -v "$PWD/notes:/notes" -v muninn-data:/data muninn-mcp
claude mcp add --transport http muninn http://127.0.0.1:8000/mcp
```

</div>

<div class="home-section">

## Twelve tools, one memory

<div class="home-grid">
<div>

| Tool | Does |
|---|---|
| `mem_search` | Hybrid, text, semantic, title or permalink search |
| `mem_read` | A note whole, by section, lines or token budget |
| `mem_write` | Create or replace, guarded by checksum |
| `mem_edit` | Append, find and replace, edit a section |
| `mem_context` | Follow relations through the graph |
| `mem_move`, `mem_delete` | A note or a whole folder |

</div>
<div>

| Tool | Does |
|---|---|
| `mem_list`, `mem_find` | Browse a folder or find files |
| `mem_project` | The project and what changed recently |
| `mem_schema` | Validate and infer frontmatter schemas |
| `mem_man` | The manual, served by the server |

Every parameter is in the [tool reference](/tools).

</div>
</div>

</div>

<div class="home-cta">

## Give your assistant a memory

<p>Muninn is free and open source under the MIT license.</p>

[Get started →](/getting-started)

</div>
