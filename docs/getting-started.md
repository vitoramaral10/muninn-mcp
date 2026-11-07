# Getting started

Muninn is an [MCP](https://modelcontextprotocol.io) server that gives an AI assistant a long-term memory: a folder of Markdown notes it can search, read and write. This page gets it running and connected in a few minutes.

## What you need

- **Docker**, to build and run the image.
- **An MCP client** that speaks streamable HTTP, such as Claude Code.
- **Optional:** an OpenAI-compatible embedding server, for search by meaning. Without one, Muninn searches by text.

## 1. Run the server

```sh
git clone https://github.com/vitoramaral10/muninn-mcp && cd muninn-mcp
docker build -t muninn-mcp .
mkdir -p notes
docker run -d --name muninn -p 127.0.0.1:8000:8000 \
  -v "$PWD/notes:/notes" -v muninn-data:/data muninn-mcp
```

Muninn now serves the notes in `./notes` at `http://127.0.0.1:8000/mcp`. The folder can start empty, or be one you already keep: every `.md` file in it is indexed.

::: warning No authentication
The MCP endpoint has no authentication. Keep it on localhost, as above, or put it behind a gateway that authenticates clients. See [Running Muninn](/operations#deploying).
:::

## 2. Connect your assistant

::: code-group

```sh [Claude Code]
claude mcp add --transport http muninn http://127.0.0.1:8000/mcp
```

```json [.mcp.json]
{
  "mcpServers": {
    "muninn": { "type": "http", "url": "http://127.0.0.1:8000/mcp" }
  }
}
```

:::

Any client that speaks streamable HTTP connects the same way. Ask the assistant to list its tools: it should see twelve, from `mem_search` to `mem_man`.

## 3. Write and find a note

Ask the assistant to remember something:

> Remember that we brew coffee at 1:16 by weight, and that pour-over beats the French press for us.

It calls `mem_write` and a file like this appears in `./notes`:

```markdown
---
title: Coffee brewing
tags: [coffee]
---

# Coffee brewing

- [ratio] 1:16 coffee to water, by weight
- [method] Pour-over gives a cleaner cup than the French press
```

In a new conversation, ask *"how strong do we make coffee?"* and it finds the note with `mem_search`. You can also edit the file by hand: Muninn picks up the change within about 15 seconds.

## 4. Add search by meaning

Point `EMBED_URL` at any OpenAI-compatible `/v1/embeddings` server. The repository's [`docker-compose.example.yml`](https://github.com/vitoramaral10/muninn-mcp/blob/main/docker-compose.example.yml) runs Muninn next to llama.cpp serving `bge-m3`:

```sh
cp docker-compose.example.yml docker-compose.yml
# put bge-m3-Q6_K.gguf in ./models, then:
docker compose up -d
```

With embeddings, `mem_search` defaults to hybrid search, which fuses text and meaning. More in [Running Muninn](/operations#the-embedding-server).

## Next steps

- **[Using Muninn](/usage):** write notes that search well, and give your assistant good instructions.
- **[Running Muninn](/operations):** configuration, backups, upgrades and troubleshooting.
- **[Tool reference](/tools):** every tool and parameter.
- **[Architecture](/architecture):** the note format, the index and how search ranks.
