#!/usr/bin/env bash
#
# Put a backup from docker-backup.sh back into a Docker Compose deployment.
#
#   ./scripts/docker-restore.sh backups/20261005-101500 --yes
#
# This replaces what is there now — the database, ~/.eren and ~/.claude in
# the container — with the backup. So, before touching anything, it takes a
# backup of the current state (labelled pre-restore), and it refuses to run
# without --yes, and refuses a folder that is not a complete backup.
#
# The deployment has to exist: on a new machine, deploy first (an empty Eren
# is fine), then restore into it. Eren is stopped while this runs and started
# again after.

set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
    sed -n '3,14p' "$0" | sed 's/^# \{0,1\}//'
}

fail() {
    echo "✗ $*" >&2
    exit 1
}

FROM=""
YES=0
while [ $# -gt 0 ]; do
    case "$1" in
        --yes) YES=1; shift ;;
        -h | --help) usage; exit 0 ;;
        -*) fail "unknown option: $1 (try --help)" ;;
        *) [ -z "$FROM" ] || fail "one backup at a time"; FROM="$1"; shift ;;
    esac
done

[ -n "$FROM" ] || { usage; exit 1; }
[ -f "$FROM/COMPLETE" ] || fail "$FROM is not a complete backup (no COMPLETE file)"
[ -s "$FROM/db.dump" ] || fail "$FROM has no database dump"
command -v docker >/dev/null 2>&1 || fail "docker is not installed"

[ "$(docker inspect -f '{{.State.Running}}' eren-postgres 2>/dev/null)" = true ] ||
    fail "eren-postgres is not running — deploy first (docker compose --profile app up -d), then restore into it"
docker inspect eren >/dev/null 2>&1 ||
    fail "there is no eren container — deploy first, then restore into it"

volume_at() {
    docker inspect -f "{{range .Mounts}}{{if and (eq .Type \"volume\") (eq .Destination \"$2\")}}{{.Name}}{{end}}{{end}}" "$1" 2>/dev/null
}

echo "This replaces the current database, ~/.eren and ~/.claude with $FROM:"
sed 's/^/  /' "$FROM/COMPLETE"
if [ "$YES" != 1 ]; then
    echo "Run again with --yes to go ahead. A backup of the current state is taken first."
    exit 1
fi

./scripts/docker-backup.sh --label pre-restore

echo "→ stopping eren"
docker stop eren >/dev/null

# Whatever happens below, Eren comes back up: a failed restore should leave a
# running Eren (and the pre-restore backup), not a stopped one.
trap 'echo "→ starting eren"; docker start eren >/dev/null' EXIT

echo "→ restoring the database"
# The schema is dropped and the dump replayed in one transaction. Dropping
# everything rather than what the dump names (pg_restore --clean) is what
# makes an older backup restore exactly: tables a later migration added would
# otherwise survive beside a migration history that says they do not exist.
# One transaction, so a restore that fails leaves the database as it was.
docker exec -i eren-postgres sh -c '
    { echo "DROP SCHEMA public CASCADE; CREATE SCHEMA public;"; pg_restore --no-owner -f -; } |
        psql -q -U "$POSTGRES_USER" -d "$POSTGRES_DB" -v ON_ERROR_STOP=1 --single-transaction >/dev/null' \
    <"$FROM/db.dump"
echo "  ✓ database"

IMAGE="$(docker inspect -f '{{.Image}}' eren)"
for pair in "state:/home/eren/.eren" "claude:/home/eren/.claude"; do
    part="${pair%%:*}"
    dest="${pair#*:}"
    [ -f "$FROM/$part.tgz" ] || { echo "  ! no $part.tgz in the backup — $dest left as it is"; continue; }
    volume="$(volume_at eren "$dest")"
    [ -n "$volume" ] || { echo "  ! nothing is mounted at $dest — skipped"; continue; }
    # Emptied first, so a file made after the backup does not survive into
    # it; the model cache the backup left out is downloaded again.
    docker run --rm -i --user 0 --entrypoint sh -v "$volume:/v" "$IMAGE" \
        -c 'find /v -mindepth 1 -delete && tar xzpf - -C /v' <"$FROM/$part.tgz"
    echo "  ✓ $dest"
done

echo "✓ restored $FROM"
