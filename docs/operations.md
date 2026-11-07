# Running Muninn

## Deploying

The image is one static binary on `scratch` (about 7 MB). It runs as user `1000:1000`, listens on port 8000, and checks its own health with `muninn-mcp healthcheck`.

[`docker-compose.example.yml`](https://github.com/vitoramaral10/muninn-mcp/blob/main/docker-compose.example.yml) runs Muninn next to an embedding server. The smallest useful setup:

```yaml
services:
  muninn:
    image: muninn-mcp:latest
    build: .
    restart: unless-stopped
    user: "1000:1000"
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    environment:
      EMBED_URL: http://embed:8080
    volumes:
      - ./notes:/notes        # the knowledge base; back this up
      - ./muninn-data:/data   # the index; derived, safe to delete
```

**What needs write access.** The notes folder needs it unless the server is read-only. `/data` must always be writable by user 1000. The image needs no other writable path, so `read_only: true` works.

**Memory.** Muninn holds every vector in memory:

- a 1,024-dimension model such as `bge-m3` takes about 4 KiB per chunk;
- a base of ~170 notes makes ~9,500 chunks and runs in about 50 MiB resident;
- a limit of 256 MiB leaves room to grow.

**Exposure.** The endpoint has no authentication. Publish it only on a private network, or put it behind a gateway that authenticates clients. See [SECURITY.md](https://github.com/vitoramaral10/muninn-mcp/blob/main/SECURITY.md).

## Configuration

| Variable | Default | |
|---|---|---|
| `MUNINN_NOTES_DIR` | `/notes` | The notes folder. Must exist at start. |
| `MUNINN_DATA_DIR` | `/data` | Where `muninn.db` lives. Created if missing. |
| `MUNINN_PROJECT` | `main` | The project's name in tool calls, and the first segment of new permalinks. |
| `MUNINN_PROJECT_ID` | | Another name accepted in `project`, such as an external id clients already use. |
| `MUNINN_READ_ONLY` | | `1` hides `mem_write`, `mem_edit`, `mem_move` and `mem_delete`. |
| `MUNINN_DEMOTE_CATEGORIES` | | Observation categories that rank after current facts, comma-separated, such as `superseded,closed`. Compared without case. |
| `MUNINN_SCAN_SECONDS` | `15` | How often the folder is compared with the index. |
| `EMBED_URL` | | Base URL of an OpenAI-compatible embedding server. Unset: text search only. |
| `EMBED_MODEL` | `bge-m3` | Model name sent to that server. It is also part of the vector cache's key. |
| `EMBED_BATCH` | `8` | Chunks per embedding request. |
| `MCP_HOST`, `MCP_PORT` | `0.0.0.0`, `8000` | Listen address. |
| `RUST_LOG` | `info` | Log level: `error`, `warn`, `info` or `debug`. |

## The embedding server

**Any server that speaks the OpenAI `POST /v1/embeddings` API works.** The examples use llama.cpp's `llama-server` with [bge-m3](https://huggingface.co/BAAI/bge-m3), which handles many languages, including Portuguese and English in the same base. A setup that works:

```sh
llama-server -m bge-m3-Q6_K.gguf --embedding --pooling cls \
  -c 4096 -b 1024 -ub 1024 --alias bge-m3 --host 0.0.0.0 --port 8080
```

**Embedding a whole base takes time on a CPU.** On an 8-thread laptop, `bge-m3` does 40 to 100 chunks a minute; a GPU (Vulkan, CUDA or Metal) is far faster. Search works from the start: text search right away, meaning search for each passage as its vector arrives.

**When the server is down or slow,** `hybrid` falls back to text search, and `semantic` returns an error that says so. A query waits at most 15 seconds for its embedding; a batch of chunks waits at most 10 minutes.

**Changing models.** Set `EMBED_MODEL` to the new name. The new model gets its own cache, and the old model's vectors stay in the database until pruned. Different models' vectors are never compared with each other.

## Building the index elsewhere

`muninn-mcp index` indexes the notes folder, embeds every chunk, and exits. It reads the same variables as the server. Use it to fill the cache on a faster machine and copy `muninn.db` to the server.

```sh
docker run --rm -v "$PWD/notes:/notes:ro" -v "$PWD/data:/data" \
  -e EMBED_URL=http://gpu-box:8080 muninn-mcp index
```

Vectors are keyed by the text of each passage, so a copied database stays valid even if some notes changed in between. The server re-indexes what differs and embeds only the new passages.

## Backups and upgrades

**Back up the notes folder; that is the whole knowledge base.** A git repository works well: every write is atomic, so a commit never catches half a file.

`muninn.db` needs no backup. Deleting it costs a re-index (seconds) and re-embedding every passage (minutes to hours, depending on the embedding server). Keeping a copy just saves that time.

**Upgrading.** Replace the image and restart. If the new version parses notes differently, it re-reads every note on start (`N notes in /notes (N re-indexed)` in the log). Vectors are reused wherever the passage text is unchanged.

## Logs

At `info` (the default; `RUST_LOG=warn` hides these):

- one line at start with the note count and how many were re-indexed;
- one line with the endpoint;
- embedding progress about every 200 chunks, then `index complete`;
- `scan: N notes changed on disk` whenever the scan picks up outside edits;
- one line per tool call, such as `call mem_search mode=text ok 41ms 2310 chars`: the tool, the mode when given, the outcome (`ok`, `empty`, `error` or `invalid`), the time and the size of the answer. Arguments are not logged, since they hold note content.

Failures go to `warn` and do not stop the server:

- **Embedding errors:** the backfill tries again on its next pass.
- **Query embedding errors:** that search runs by text only.
- **Scan errors:** the next scan runs as usual.

## Troubleshooting

| Symptom | Check |
|---|---|
| `semantic` says the embedding server did not answer | `EMBED_URL` is reachable from the container. The server was started with `--embedding`. |
| Meaning search misses a note that was just written | Its passages are still waiting to be embedded. `mem_project` shows how many chunks have vectors. |
| An edit made outside Muninn does not show up | Wait one `MUNINN_SCAN_SECONDS`. Make sure the file is not hidden and not behind a symlink. |
| `MUNINN_NOTES_DIR ... is not a folder` at start | The notes volume is not mounted where the variable points. |
| `unable to open database file` | `/data` is not writable by user 1000. |
| A write tool is "not found" | The server runs with `MUNINN_READ_ONLY=1`. |
