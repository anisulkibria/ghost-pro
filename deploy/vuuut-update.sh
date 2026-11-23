#!/usr/bin/env bash
# Build a Vuuut update the console asked for.
#
# Run as root by vuuut-update.service, which vuuut-update.path starts when
# the console writes $UPDATE_DIR/request.json. This script only fetches,
# syncs and builds; the console's worker then restarts every site on the new
# binary, which is what applies the database migrations (see
# crates/pro-web/src/updates.rs).
#
# Reports every step in $UPDATE_DIR/status.json and writes its output to
# $UPDATE_DIR/update.log, both readable by the console.
set -uo pipefail

UPDATE_DIR=${UPDATE_DIR:-/opt/Ghost/vuuut-pro/run/data/updates}
VUUUT_DIR=${VUUUT_DIR:-/opt/Ghost/vuuut}
CLONE_DIR=${CLONE_DIR:-/opt/Ghost/update/vuuut}
RUN_USER=${RUN_USER:-vuuut}
CARGO_ENV=${CARGO_ENV:-/root/.cargo/env}

REQUEST=$UPDATE_DIR/request.json
STATUS=$UPDATE_DIR/status.json
LOG=$UPDATE_DIR/update.log

[ -f "$REQUEST" ] || exit 0

# Claim the request and read it, then delete it: it carries the token.
CLAIMED=$UPDATE_DIR/request.claimed.$$
mv "$REQUEST" "$CLAIMED" || exit 0
field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2], ""))' "$CLAIMED" "$1"; }
ID=$(field id); REPO=$(field repository); BRANCH=$(field branch); TOKEN=$(field token)
rm -f "$CLAIMED"

: > "$LOG"
chown "$RUN_USER": "$LOG" 2>/dev/null
exec >>"$LOG" 2>&1

# status STATE STEP [MESSAGE] [COMMIT]: merge into status.json, atomically.
status() {
    python3 - "$STATUS" "$ID" "$1" "$2" "${3:-}" "${4:-}" <<'PY'
import json, os, sys, datetime
path, rid, state, step, message, commit = sys.argv[1:7]
try:
    s = json.load(open(path))
except Exception:
    s = {}
if s.get("id") not in ("", None, rid):
    s = {}
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
s.update({"id": rid, "state": state, "step": step, "updated_at": now})
if message:
    s["message"] = message
if commit:
    s["commit"] = commit
if state in ("failed",):
    s["finished_at"] = now
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(s, f, indent=2)
os.rename(tmp, path)
PY
    chown "$RUN_USER": "$STATUS" 2>/dev/null
    echo "== $(date -u +%H:%M:%S) $1: $2"
}

fail() {
    status failed "$1" "$2"
    exit 1
}

# The console validates these too; this script trusts nothing it is handed.
[[ $REPO =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail "Checking the request" "Repository '$REPO' is not owner/name."
[[ $BRANCH =~ ^[A-Za-z0-9_./-]+$ && $BRANCH != *..* && $BRANCH != -* ]] || fail "Checking the request" "Branch '$BRANCH' is not a valid branch name."

# ---- fetch ------------------------------------------------------------------
status fetching "Fetching $REPO ($BRANCH) from GitHub"
export GIT_TERMINAL_PROMPT=0
# The token goes in through the environment, not the command line, so it
# never shows up in `ps`.
if [ -n "$TOKEN" ]; then
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader
    export GIT_CONFIG_VALUE_0="Authorization: Basic $(printf 'x-access-token:%s' "$TOKEN" | base64 -w0)"
fi
URL="https://github.com/$REPO.git"
mkdir -p "$(dirname "$CLONE_DIR")"
if [ ! -d "$CLONE_DIR/.git" ]; then
    rm -rf "$CLONE_DIR"
    git init -q "$CLONE_DIR" || fail "Fetching from GitHub" "Could not create $CLONE_DIR."
fi
git -C "$CLONE_DIR" remote remove origin 2>/dev/null
git -C "$CLONE_DIR" remote add origin "$URL"
git -C "$CLONE_DIR" fetch --depth 50 origin "$BRANCH" \
    || fail "Fetching from GitHub" "git fetch failed. Is the repository right, and does the token in Settings → Updates have read access?"
COMMIT=$(git -C "$CLONE_DIR" rev-parse FETCH_HEAD)
git -C "$CLONE_DIR" checkout -q -f --detach "$COMMIT" || fail "Fetching from GitHub" "Could not check out $COMMIT."
git -C "$CLONE_DIR" clean -q -fdx
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 TOKEN
echo "commit $COMMIT"

# ---- sync -------------------------------------------------------------------
status syncing "Copying $(echo "$COMMIT" | cut -c1-7) into $VUUUT_DIR" "" "$COMMIT"
# The checkout's build output, site content and private settings are not in
# git and must survive; everything else is made to match the commit.
rsync -a --delete --exclude '/target/' --exclude '/content/' --exclude '/.env' --exclude '/.git/' \
    "$CLONE_DIR/" "$VUUUT_DIR/" || fail "Copying the source" "rsync into $VUUUT_DIR failed."
# Bundled themes are what new sites start from; add and update, never delete
# a theme that only exists on this server.
if [ -d "$CLONE_DIR/content/themes" ]; then
    mkdir -p "$VUUUT_DIR/content/themes"
    rsync -a "$CLONE_DIR/content/themes/" "$VUUUT_DIR/content/themes/" || fail "Copying the source" "rsync of the bundled themes failed."
fi

# ---- build ------------------------------------------------------------------
status building "Building the release binary (this takes a while)" "" "$COMMIT"
# shellcheck disable=SC1090
[ -f "$CARGO_ENV" ] && . "$CARGO_ENV"
BIN=$VUUUT_DIR/target/release/vuuut
[ -f "$BIN" ] && cp -p "$BIN" "$BIN.prev"
( cd "$VUUUT_DIR" && nice -n 10 cargo build --release -p vuuut --bin vuuut ) \
    || fail "Building" "cargo build failed; the sites are still on the previous build. See the log."
[ -x "$BIN" ] || fail "Building" "The build finished but $BIN is missing."

# The console's worker takes it from here: restart and migrate every site.
status built "Build finished; restarting sites" "" "$COMMIT"
