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
#
# It needs only docker-compose.yml, .env and this script, laid out as in the
# repository — a server needs no source and no toolchain. In .env:
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
    sed -n '3,26p' "$0" | sed 's/^# \{0,1\}//'
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

while [ $# -gt 0 ]; do
    case "$1" in
        --tag) TAG="${2:?--tag needs a value}"; shift 2 ;;
        --with-storage) PROFILES+=(--profile storage); shift ;;
        # The same as COMPOSE_FILE in .env, which compose reads by itself.
        --with-previews) export COMPOSE_FILE=docker-compose.yml:docker-compose.previews.yml; shift ;;
        --down) DOWN=1; shift ;;
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
    # Every profile, so a storage container started earlier stops too.
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
echo "  logs: docker compose logs -f eren"
