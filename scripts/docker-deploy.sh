#!/usr/bin/env bash
#
# Run Eren and its Postgres with Docker Compose from a published image — the
# one scripts/docker-publish.sh pushed — instead of building from source.
#
#   ./scripts/docker-deploy.sh                  # pull neiellcare71/eren:latest and (re)start
#   ./scripts/docker-deploy.sh --tag 1a2b3c4    # a specific build (roll back the same way)
#   ./scripts/docker-deploy.sh --with-storage   # also object storage for KB attachments
#   ./scripts/docker-deploy.sh --with-previews  # also the Docker socket, for previews (README first)
#   ./scripts/docker-deploy.sh --down           # stop it (volumes, and your data, are kept)
#   ./scripts/docker-deploy.sh --no-backup      # skip the backup taken before every deploy
#   ./scripts/docker-deploy.sh --force          # deploy past the two checks below
#
# Before it changes anything it backs up the running deployment
# (docker-backup.sh, into backups/), and it refuses to deploy when doing so
# would leave your data behind: when the compose project name changed (fresh,
# empty volumes beside the old ones), or when EREN_PROJECTS_DIR changed (every
# project and worktree path Eren stored would point at nothing).
#
# It needs only docker-compose.yml, .env, this script and docker-backup.sh,
# laid out as in the repository — a server needs no source and no toolchain.
# In .env:
#
#   EREN_IMAGE=you/eren                # what docker-publish.sh pushed; default neiellcare71/eren
#   CLAUDE_CODE_OAUTH_TOKEN=…          # from `claude setup-token`
#   EREN_PROJECTS_DIR=/home/you/code   # the code agents work on
#
# A name with a namespace (you/eren) is pulled from its registry. A plain one
# (EREN_IMAGE=eren) is one no registry holds: it has to be in this Docker
# already — `./scripts/docker-publish.sh eren` puts it there.
#
# To deploy to another machine from this one, point Docker at it first:
#   DOCKER_HOST=ssh://you@server ./scripts/docker-deploy.sh
# (.env is read here; EREN_PROJECTS_DIR is then a path on the server.)

set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
    sed -n '3,34p' "$0" | sed 's/^# \{0,1\}//'
}

fail() {
    echo "✗ $*" >&2
    exit 1
}

# A setting from the environment, else from .env. Read as a value, never
# sourced: .env holds a credential and is not a script.
setting() {
    local name="$1"
    if [ -n "${!name:-}" ]; then
        printf '%s' "${!name}"
    elif [ -f .env ]; then
        sed -n "s/^[[:space:]]*$name=//p" .env | tail -n 1 | sed -e 's/^["'\'']//' -e 's/["'\'']$//'
    fi
}

PROFILES=(--profile app)
DOWN=0
TAG=""
BACKUP=1
FORCE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --tag) TAG="${2:?--tag needs a value}"; shift 2 ;;
        --with-storage) PROFILES+=(--profile storage); shift ;;
        # The same as COMPOSE_FILE in .env, which compose reads by itself.
        --with-previews) export COMPOSE_FILE=docker-compose.yml:docker-compose.previews.yml; shift ;;
        --down) DOWN=1; shift ;;
        --no-backup) BACKUP=0; shift ;;
        --force) FORCE=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) fail "unknown option: $1 (try --help)" ;;
    esac
done

command -v docker >/dev/null 2>&1 || fail "docker is not installed"
docker compose version >/dev/null 2>&1 || fail "docker compose (v2) is not available"
[ -f docker-compose.yml ] || fail "docker-compose.yml is not next to scripts/ ($(pwd))"
case "${COMPOSE_FILE:-$(setting COMPOSE_FILE)}" in
    *docker-compose.previews.yml*)
        [ -f docker-compose.previews.yml ] ||
            fail "COMPOSE_FILE asks for docker-compose.previews.yml, which is not next to docker-compose.yml"
        ;;
esac

if [ "$DOWN" = 1 ]; then
    # Every profile, so a storage container started earlier stops too. Never
    # -v: that is the one flag that deletes the volumes, and your data with them.
    docker compose --profile app --profile storage down
    exit 0
fi

IMAGE="$(setting EREN_IMAGE)"
export EREN_IMAGE="${IMAGE:-neiellcare71/eren}"
export EREN_TAG="${TAG:-$(setting EREN_TAG)}"
EREN_TAG="${EREN_TAG:-latest}"

# Checked for presence only; the token's value is never printed.
[ -n "$(setting CLAUDE_CODE_OAUTH_TOKEN)" ] ||
    echo "! CLAUDE_CODE_OAUTH_TOKEN is not set: Eren will start, and every Claude Code run will report \"not logged in\". Run \`claude setup-token\` and put it in .env."
[ -n "$(setting EREN_PROJECTS_DIR)" ] ||
    echo "! EREN_PROJECTS_DIR is not set: agents can only see /workspace inside the container."

# ── Nothing left behind ────────────────────────────────────────────────────
# Both checks compare what is running with what this deploy would start.
# Neither failure loses anything by itself — the old volumes and folders are
# all still there — but a deploy past them comes up looking empty, which is
# how data gets "lost" and then deleted while cleaning up.
refuse() {
    if [ "$FORCE" = 1 ]; then
        echo "! $1 — deploying anyway (--force)"
    else
        fail "$1

  Nothing has been changed. If this is really what you want, run again with --force."
    fi
}

# 1. Compose names volumes <project>_<volume>, and the project is the folder's
#    name unless COMPOSE_PROJECT_NAME says otherwise. Deploy from a copy in
#    another folder and every volume is new and empty, beside the real ones.
PROJECT="$(docker compose "${PROFILES[@]}" config 2>/dev/null | sed -n 's/^name: //p' | head -n 1)"
[ -n "$PROJECT" ] || fail "could not read the compose project name (docker compose config failed)"
while IFS= read -r key; do
    [ -n "$key" ] || continue
    docker volume inspect "${PROJECT}_${key}" >/dev/null 2>&1 && continue
    OTHERS="$(docker volume ls -q | grep -E "_${key}\$" | grep -vx "${PROJECT}_${key}" || true)"
    if [ -n "$OTHERS" ]; then
        OTHER_PROJECT="$(printf '%s\n' "$OTHERS" | head -n 1)"
        OTHER_PROJECT="${OTHER_PROJECT%_"$key"}"
        refuse "this deploy would create a new, empty ${PROJECT}_${key}, but your data is in $(echo $OTHERS).
  The compose project is called \"$PROJECT\" here and was \"$OTHER_PROJECT\" before.
  Put COMPOSE_PROJECT_NAME=$OTHER_PROJECT in .env (or deploy from the same folder as before)."
    fi
done < <(docker compose "${PROFILES[@]}" config --volumes 2>/dev/null)

# 2. The projects folder is mounted at the same path inside and out, and that
#    path is stored: every project, worktree, and git's own worktree links.
if docker inspect eren >/dev/null 2>&1; then
    WAS="$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' eren | sed -n 's/^EREN_BROWSE_ROOT=//p' | head -n 1)"
    WILL="$(docker compose "${PROFILES[@]}" config --format json 2>/dev/null |
        grep -o '"EREN_BROWSE_ROOT": *"[^"]*"' | head -n 1 | sed 's/.*: *"\(.*\)"/\1/')"
    if [ -n "$WAS" ] && [ -n "$WILL" ] && [ "$WAS" != "$WILL" ]; then
        refuse "EREN_PROJECTS_DIR was $WAS and would now be $WILL.
  Every project Eren knows is stored under $WAS; they would all point at nothing.
  Set EREN_PROJECTS_DIR=$WAS in .env, or move the folder and keep the same path."
    fi
fi

if [[ "$EREN_IMAGE" == */* ]]; then
    echo "→ pulling $EREN_IMAGE:$EREN_TAG"
    docker compose "${PROFILES[@]}" pull
else
    # No registry has it; pull everything else and use the local build.
    docker image inspect "$EREN_IMAGE:$EREN_TAG" >/dev/null 2>&1 ||
        fail "there is no $EREN_IMAGE:$EREN_TAG image in this Docker — build it with ./scripts/docker-publish.sh $EREN_IMAGE, or set EREN_IMAGE to one you pushed (you/eren)"
    echo "→ using the local $EREN_IMAGE:$EREN_TAG; pulling the rest"
    SERVICES=()
    while IFS= read -r service; do
        [ "$service" = eren ] || SERVICES+=("$service")
    done < <(docker compose "${PROFILES[@]}" config --services)
    docker compose "${PROFILES[@]}" pull ${SERVICES[@]+"${SERVICES[@]}"}
fi

# The backup goes last, after everything that could still refuse: it is
# what is running now, the moment before it is replaced.
if [ "$BACKUP" = 1 ]; then
    if [ "$(docker inspect -f '{{.State.Running}}' eren-postgres 2>/dev/null)" = true ]; then
        [ -x scripts/docker-backup.sh ] ||
            fail "scripts/docker-backup.sh is missing — copy it next to this script, or pass --no-backup"
        ./scripts/docker-backup.sh --label pre-deploy
    else
        echo "→ no running deployment to back up"
    fi
fi

echo "→ starting"
# --no-build: use what was pulled, never fall back to building from source.
docker compose "${PROFILES[@]}" up -d --no-build --wait

# Where Docker actually published it, not where it was meant to be: a
# compose file from before EREN_PUBLISH_IP existed still says 127.0.0.1.
PUBLISHED="$(docker compose "${PROFILES[@]}" port eren 4820 2>/dev/null | head -n 1)"
PUBLISHED="${PUBLISHED:-127.0.0.1:$(setting EREN_PORT)}"
PORT="${PUBLISHED##*:}"
case "$PUBLISHED" in
    127.0.0.1:* | "[::1]:"*)
        echo "✓ Eren $EREN_IMAGE:$EREN_TAG is running on http://127.0.0.1:$PORT (this machine only)"
        echo "  To reach it from other devices: EREN_PUBLISH_IP=0.0.0.0 and EREN_ALLOWED_HOSTS=<this machine's address> in .env,"
        echo "  then run this again. If the port above still says 127.0.0.1, docker-compose.yml predates EREN_PUBLISH_IP."
        ;;
    *)
        echo "✓ Eren $EREN_IMAGE:$EREN_TAG is published on $PUBLISHED"
        HOSTS="$(setting EREN_ALLOWED_HOSTS)"
        if [ -n "$HOSTS" ]; then
            for host in ${HOSTS//,/ }; do echo "  open http://$host:$PORT"; done
        else
            echo "! EREN_ALLOWED_HOSTS is not set: other devices are refused by name until it lists the address they use."
        fi
        ;;
esac
# The token is on unless .env turns it off, and the host's own browser
# arrives through Docker's gateway, so every browser needs the link once.
TOKEN_SETTING="$(setting EREN_ACCESS_TOKEN)"
if [ "$(printf '%s' "$TOKEN_SETTING" | tr '[:upper:]' '[:lower:]')" != "off" ]; then
    echo "  Each browser opens Eren's access link once: docker compose logs eren | grep 'open this link'"
fi
echo "  logs: docker compose logs -f eren"
