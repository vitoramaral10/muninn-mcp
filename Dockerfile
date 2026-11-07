# syntax=docker/dockerfile:1
# Static musl binary on an empty base: no shell, no libc, no package manager.
# SQLite is compiled in from source.
FROM rust:1.99.1-alpine3.24@sha256:27cd9befd4a05d6f6eb407c3e28365f5092f008bfc5946207cca76ae5179ef23 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/muninn-mcp /muninn-mcp \
 && mkdir -m 1777 /scratch-tmp

FROM scratch
LABEL org.opencontainers.image.description="MCP server for a Markdown knowledge base, in a single static binary" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /muninn-mcp /muninn-mcp
# SQLite needs a temp dir for large sorts; scratch has none.
COPY --from=build /scratch-tmp /tmp
ENV MUNINN_NOTES_DIR=/notes \
    MUNINN_DATA_DIR=/data \
    TMPDIR=/tmp
# The notes folder is a bind mount owned by 1000:1000.
USER 1000:1000
EXPOSE 8000
HEALTHCHECK --interval=60s --timeout=10s --start-period=30s CMD ["/muninn-mcp", "healthcheck"]
ENTRYPOINT ["/muninn-mcp"]
