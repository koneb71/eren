# Eren in a container.
#
# Read the "Running in Docker" section of README.md before using this. The
# short version: Eren works by spawning the official `claude` CLI, so the
# container needs both the CLI and a way to authenticate. Inside a container
# there is no keychain and no browser, which leaves exactly one option — a
# long-lived token from `claude setup-token`, supplied as an environment
# variable. That is a real trade-off, not a detail.

# ── 1. Dashboard ───────────────────────────────────────────────────────────
FROM node:22-slim AS web
# Laid out as the repository is, under /src, because two web tests import a
# specification the Rust tests read too (`../../../crates/...`), and
# `pnpm build` type-checks the tests. Only those files are copied from
# crates/, so a Rust edit does not rebuild the dashboard. A test in
# eren-cli (docs_tests) fails when a new one is imported and not listed here.
WORKDIR /src/web
RUN corepack enable
# Manifests first so a source-only edit doesn't reinstall the world.
COPY web/package.json web/pnpm-lock.yaml web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY crates/eren-core/src/apps/expr_cases.json /src/crates/eren-core/src/apps/expr_cases.json
COPY crates/eren-core/src/runs/mention_cases.json /src/crates/eren-core/src/runs/mention_cases.json
COPY web/ ./
RUN pnpm build

# ── 2. Server ──────────────────────────────────────────────────────────────
# Pinned to the same Debian release as the runtime stage below. A newer
# builder links against a newer glibc and the binary won't start.
#
# Trixie, not bookworm: the onnxruntime that `ort` downloads (fastembed's, for
# the knowledge base) is a static library built with GCC 14, and bookworm's
# GCC 12 libstdc++ lacks symbols it needs, so the link fails there.
FROM rust:1-slim-trixie AS server
WORKDIR /src
# g++: the tree-sitter grammars and onnxruntime are C++, and the slim image
# has a C compiler but not libstdc++ to link them against.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev g++ \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release --locked -p eren-cli

# ── 3. Runtime ─────────────────────────────────────────────────────────────
FROM debian:trixie-slim
# Which Claude Code the image carries. Pinned, so a rebuild is reproducible —
# and it matters more than a version usually does: the model picker's
# "latest" choices are aliases this CLI resolves, so a newer model reaches
# the container only through a newer CLI. Raise it here, or per build with
# `--build-arg CLAUDE_CODE_VERSION=…` (docker-publish.sh passes it through).
ARG CLAUDE_CODE_VERSION=2.1.259
# git: worktrees are the whole isolation model. node: the CLI ships as an npm
# package. ca-certificates: the CLI talks to Anthropic over TLS.
#
# The Docker CLI with its compose and buildx plugins, for previews and
# container apps — the client only, no daemon. It does nothing until
# docker-compose.previews.yml hands the container the host's socket, and
# that is a decision, not a default: read "Previews in Docker" in README.md.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates curl gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION} \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce-cli docker-compose-plugin docker-buildx-plugin \
    && apt-get purge -y --auto-remove curl gnupg \
    && rm -rf /var/lib/apt/lists/*

# Who commits. Landing a card squash-merges and commits in your repository,
# and that commit takes git's configured identity; a container has none, so
# git refused ("Author identity unknown") — and one set by hand in the
# terminal lived in the container's home and went with the next redeploy.
# A system default, the same identity Eren's own commits name: a repository's
# own `git config user.name/user.email`, kept in its .git/config, still wins.
RUN git config --system user.name eren \
    && git config --system user.email eren@localhost

# Runs as a normal user: an agent with a shell should not be uid 0, and the
# uid is overridable so files it writes into your mounted code stay yours.
ARG UID=1000
ARG GID=1000
RUN groupadd -g "${GID}" eren 2>/dev/null || true \
    && useradd -m -u "${UID}" -g "${GID}" -s /bin/bash eren 2>/dev/null || true \
    && install -d -o "${UID}" -g "${GID}" /home/eren/.eren /home/eren/.claude
# ~/.claude is made here, owned by that user, because compose mounts a volume
# on it and a volume takes the ownership of the directory it lands on — one
# Docker had to create would be root's, and the CLI could not write to it.
#
# The project was called aichip, and its state volume was mounted at
# /home/aichip/.aichip. The same volume is now mounted at ~/.eren, and paths
# stored before the rename — worktrees in the database, git's own worktree
# links — still name the old place, so the old place points at the new one.
RUN mkdir -p /home/aichip && ln -s /home/eren/.eren /home/aichip/.aichip

COPY --from=server /src/target/release/eren /usr/local/bin/eren
COPY --from=web /src/web/dist /srv/eren/web

ENV EREN_WEB_DIST=/srv/eren/web
# Bind wide inside the container: the container's own loopback is not the
# host's, so nothing outside the namespace could reach it otherwise. What is
# actually exposed is decided by the port mapping you declare in compose — and
# `-p 4820:4820` publishes on every interface, so it is reachable from your
# network — and since the access token is on unless EREN_ACCESS_TOKEN=off,
# every caller then needs the access link `docker logs` shows. Compose does
# the same: the dashboard on every interface, the token on.
ENV EREN_BIND=0.0.0.0
# Acknowledged here because binding wide is the only way a container can work,
# not because the exposure is smaller. See `eren_server::exposure`.
ENV EREN_TRUST_NETWORK=1
EXPOSE 4820

USER eren
WORKDIR /home/eren
ENTRYPOINT ["eren"]
CMD ["serve", "--headless"]
