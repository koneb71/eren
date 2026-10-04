#!/usr/bin/env bash
#
# Back up a Docker Compose deployment of Eren: everything a redeploy must not
# lose, into one folder on the machine running this script.
#
#   ./scripts/docker-backup.sh                 # back up now
#   ./scripts/docker-backup.sh --label pre-x   # name it (the folder gets the label too)
#
# Each backup is a folder under backups/ (EREN_BACKUP_DIR to change it):
#
#   db.dump       the database — projects, chats and their messages, cards,
#                 agents, settings; `pg_dump -Fc`, taken while it runs
#   state.tgz     ~/.eren in the container — worktrees, attachments, apps,
#                 spaces (the re-downloadable model cache is left out)
#   claude.tgz    ~/.claude in the container — the CLI sessions chats resume
#
# The newest EREN_BACKUP_KEEP (default 10) are kept; older ones are removed.
# docker-deploy.sh runs this before every deploy. Put a backup back with
# docker-restore.sh.
#
# Everything is found from the running containers rather than from names
# written here: the volumes are whichever ones `eren` and `eren-postgres`
# actually have mounted, so a renamed compose project is backed up as it is.
# With DOCKER_HOST=ssh://…, the backup is streamed back to this machine —
# which is also the point: a copy that is not on the same disk.

set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
    sed -n '3,25p' "$0" | sed 's/^# \{0,1\}//'
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

LABEL=""
while [ $# -gt 0 ]; do
    case "$1" in
        --label) LABEL="${2:?--label needs a value}"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) fail "unknown option: $1 (try --help)" ;;
    esac
done
case "$LABEL" in
    *[!A-Za-z0-9._-]*) fail "--label takes letters, digits, '.', '_' and '-' only" ;;
esac

command -v docker >/dev/null 2>&1 || fail "docker is not installed"

running() {
    [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = true ]
}

# The name of the volume container $1 has mounted at $2, if it is a volume.
volume_at() {
    docker inspect -f "{{range .Mounts}}{{if and (eq .Type \"volume\") (eq .Destination \"$2\")}}{{.Name}}{{end}}{{end}}" "$1" 2>/dev/null
}

running eren-postgres || fail "eren-postgres is not running, so there is nothing to back up from (start it with docker compose up -d)"

DIR="${EREN_BACKUP_DIR:-$(setting EREN_BACKUP_DIR)}"
DIR="${DIR:-backups}"
NAME="$(date +%Y%m%d-%H%M%S)${LABEL:+-$LABEL}"
OUT="$DIR/$NAME"
mkdir -p "$OUT"
# Holds the database: settings, check commands, every conversation. Readable
# by you alone, like .env.
chmod 700 "$DIR" "$OUT"

# Written next to the files, so a half-finished backup is never mistaken for
# a whole one: restore refuses a folder without it.
trap 'echo "✗ backup incomplete; $OUT is not usable" >&2' ERR

echo "→ backing up to $OUT"

# The container's own variables name the role and database, so nothing here
# has to repeat what compose (or .env) said they are.
docker exec eren-postgres sh -c 'exec pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" -Fc' >"$OUT/db.dump"
[ -s "$OUT/db.dump" ] || fail "pg_dump wrote nothing"
echo "  ✓ database ($(du -h "$OUT/db.dump" | cut -f1))"

# The volumes, read through a throwaway container of Eren's own image (which
# has tar, and is already here) as root, so every file is readable. Streamed
# to stdout rather than through a bind mount: it works the same over
# DOCKER_HOST=ssh, and the files are written by you, not by root.
if docker inspect eren >/dev/null 2>&1; then
    IMAGE="$(docker inspect -f '{{.Image}}' eren)"
    for pair in "state:/home/eren/.eren" "claude:/home/eren/.claude"; do
        part="${pair%%:*}"
        dest="${pair#*:}"
        volume="$(volume_at eren "$dest")"
        if [ -z "$volume" ]; then
            echo "  ! nothing is mounted at $dest — skipped (a compose file from before it was a volume?)"
            continue
        fi
        docker run --rm --user 0 --entrypoint tar -v "$volume:/v:ro" "$IMAGE" \
            czf - -C /v --exclude=./models --exclude=./tmp --exclude=./pgdata . >"$OUT/$part.tgz"
        echo "  ✓ $dest ($(du -h "$OUT/$part.tgz" | cut -f1), volume $volume)"
    done
else
    echo "  ! there is no eren container: only the database was backed up"
fi

{
    echo "taken=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "postgres_volume=$(volume_at eren-postgres /var/lib/postgresql/data)"
    echo "image=$(docker inspect -f '{{.Config.Image}}' eren 2>/dev/null || true)"
} >"$OUT/COMPLETE"
trap - ERR

# Retention: only complete folders this script made (a timestamp first),
# newest kept. One without COMPLETE was cut short and is not a backup, so it
# neither takes a slot from a good one nor is removed in its place.
KEEP="${EREN_BACKUP_KEEP:-$(setting EREN_BACKUP_KEEP)}"
KEEP="${KEEP:-10}"
case "$KEEP" in '' | *[!0-9]*) fail "EREN_BACKUP_KEEP must be a number" ;; esac
if [ "$KEEP" -gt 0 ]; then
    find "$DIR" -mindepth 1 -maxdepth 1 -type d -name '20[0-9][0-9][0-9][0-9][0-9][0-9]-[0-9][0-9][0-9][0-9][0-9][0-9]*' |
        sort -r | while IFS= read -r d; do [ -f "$d/COMPLETE" ] && echo "$d"; done |
        tail -n +"$((KEEP + 1))" | while IFS= read -r old; do
            rm -rf -- "$old"
            echo "  − removed old backup $old"
        done
fi

echo "✓ backup complete: $OUT"
