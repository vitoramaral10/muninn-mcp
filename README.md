# Muninn

[![ci](https://github.com/vitoramaral10/muninn-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/vitoramaral10/muninn-mcp/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A long-term memory for AI assistants, kept as plain Markdown files.** Muninn is an [MCP](https://modelcontextprotocol.io) server in one static Rust binary. It searches, reads and writes a folder of Markdown notes through twelve `mem_*` tools.

Muninn is Odin's raven of memory.

## Why

- **Your notes stay yours.** Each note is a Markdown file with YAML frontmatter, readable in any editor and versionable with git. The index next to them is derived: delete it and it is rebuilt.
- **Search that understands the question.** Hybrid search fuses full-text (BM25, accent-insensitive) with embeddings from any OpenAI-compatible server, by reciprocal rank. "How do we ship the API?" finds the note that only says "deploy the backend with docker compose".
- **A knowledge graph from plain lists.** `- [decision] Use Postgres #db` is an observation, and `- depends_on [[Auth service]]` is a relation. `mem_context` walks the graph in both directions.
- **Small and quick.** A 7 MB image that runs in about 50 MiB. On one 170-note base, hybrid search answers in 0.6 s.
- **Edits from anywhere.** A scan every 15 seconds picks up changes made by an editor, `git pull` or a sync tool. Writes through the tools are atomic and guarded by checksums.

## Quick start

```sh
git clone https://github.com/vitoramaral10/muninn-mcp && cd muninn-mcp
docker build -t muninn-mcp .
mkdir -p notes
docker run -d --name muninn -p 127.0.0.1:8000:8000 \
  -v "$PWD/notes:/notes" -v muninn-data:/data muninn-mcp
```

This serves text search over `./notes` at `http://127.0.0.1:8000/mcp` (streamable HTTP). For meaning search, add an embedding server and set `EMBED_URL`. [`docker-compose.example.yml`](docker-compose.example.yml) runs both together.

Then connect a client. With Claude Code:

```sh
claude mcp add --transport http muninn http://127.0.0.1:8000/mcp
```

Any MCP client that speaks streamable HTTP works the same way. Then see [Using Muninn](docs/usage.md) for how to write and find notes.

## Tools

| Tool | Does |
|---|---|
| `mem_search` | Ranked search: hybrid, text, semantic, title or permalink. Frontmatter filters, `since`, observations and relations, grep-style `lines`. |
| `mem_read` | A note or file, whole or by section, line range or token budget. |
| `mem_write` | Create or replace a note, optionally only if unchanged since read. |
| `mem_edit` | Append, prepend, find and replace, replace or insert around a section, set frontmatter. |
| `mem_move`, `mem_delete` | A note or a whole folder. |
| `mem_list`, `mem_find` | One folder, or a recursive search by file name or frontmatter. |
| `mem_context` | A note with its observations and the notes its relations reach. |
| `mem_project` | The project, and what changed recently. |
| `mem_schema` | Validate, infer and diff frontmatter schemas (Picoschema). |
| `mem_man` | The manual, built from the tool descriptions. |

Every parameter is listed in the **[tool reference](docs/tools.md)**.

## Documentation

- **[Using Muninn](docs/usage.md):** writing notes that search well, finding, reading and changing them, and instructions for your assistant.
- **[Running Muninn](docs/operations.md):** deployment, configuration, the embedding server, backups, upgrades, troubleshooting.
- **[Architecture](docs/architecture.md):** the note format, the index, how search ranks, writes and safety.
- **[Tool reference](docs/tools.md):** every tool and parameter.
- **[Contributing](CONTRIBUTING.md)** and **[security](SECURITY.md)**.

## Configuration

The common settings:

| Variable | Default | |
|---|---|---|
| `MUNINN_NOTES_DIR` | `/notes` | The notes folder. |
| `MUNINN_DATA_DIR` | `/data` | Where the index `muninn.db` lives. |
| `MUNINN_PROJECT` | `main` | The project's name in tool calls and permalinks. |
| `MUNINN_READ_ONLY` | | `1` hides the four write tools. |
| `EMBED_URL` | | OpenAI-compatible `/v1/embeddings` server. Unset: text search only. |

All the variables are in [Running Muninn](docs/operations.md#configuration).

## Security

The MCP endpoint has no authentication. Bind it to localhost, or put it behind a gateway that authenticates clients. Paths from clients cannot leave the notes folder. See [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE).
