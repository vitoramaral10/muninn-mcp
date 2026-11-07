# Security

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's [private vulnerability reporting](https://github.com/vitoramaral10/muninn-mcp/security/advisories/new), not in a public issue. Include what an attacker can do and how to reproduce it. You should get an answer within a week.

Only the latest release is supported.

## The security model

**The MCP endpoint has no authentication.** Anyone who can reach port 8000 can read every note and, unless the server is read-only, change or delete them. Deploy it one of these ways:

- on localhost, for a client on the same machine;
- on a private network that only trusted clients reach;
- behind a gateway or reverse proxy that authenticates clients (OAuth, API keys, mTLS) and forwards to Muninn.

**Paths cannot leave the notes folder.** Before touching the filesystem, every path from a client is checked. It may not contain:

- a `..` segment;
- a hidden segment (`.git`, `.obsidian`);
- a backslash or a NUL byte;
- a symlink anywhere along it.

The folder scan never follows symlinks either.

**Read-only mode** (`MUNINN_READ_ONLY=1`) removes the write tools from the server, so they cannot be listed or called. Mount the notes read-only too, so the kernel enforces it as well.

**The container** runs as an unprivileged user (1000:1000) on a `scratch` image with no shell, and works with a read-only root filesystem and every capability dropped. See the [deployment example](docs/operations.md#deploying).

**Note content reaches the model.** Muninn returns notes as written. A note that holds instructions (a prompt injection pasted from the web, say) reaches the model like any other text. Treat writes to the knowledge base from untrusted sources with the same care as the sources themselves.
