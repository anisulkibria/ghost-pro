#!/usr/bin/env bash
# Vuuut-pro control script
#   ./run.sh start|stop|restart|status|build|rebuild|test|log|doctor|tenants|staff|reap
#
# Deliberately shaped like Vuuut's own run.sh, so moving between the two
# projects needs no second vocabulary. Two things differ, and both come from
# what this process *is*:
#
#   * It runs from a deployment directory, not the repository root. `pro.json`
#     and `data/` are read from the working directory, so the platform is
#     started inside one — `run/` here. Set PRO_DIR to point somewhere else,
#     and two deployments on one machine stay independent, pidfile and all.
#
#   * Tenant sites are child processes of it. SIGTERM makes it stop each one
#     politely so every tenant closes its SQLite database cleanly; SIGKILL
#     does not, and leaves Ghost processes running on the tenant ports holding
#     databases open. So `stop` waits generously for SIGTERM, and only forces
#     as a last resort — then says what it orphaned and how to clear it.
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$(pwd)

BIN=$ROOT/target/release/ghost-pro
# Where pro.json and data/ live. The platform reads its config from the
# working directory, which is why this is a directory and not a file path.
PRO_DIR=${PRO_DIR:-$ROOT/run}
PIDFILE=$PRO_DIR/.ghost-pro.pid
LOG=$PRO_DIR/server.log

# Tenant processes bind loopback ports in this range. Only used to find
# orphans — the platform itself assigns them from pro.json.
TENANT_PORTS=${PRO_TENANT_PORTS:-21000-28999}

# ---------------------------------------------------------------------------
# What port is the edge on
# ---------------------------------------------------------------------------
#
# The answer lives in pro.json (`server.port`), with the same defaults the Rust
# loader applies: 443 with TLS on, 8080 without. PRO_SERVER__PORT wins over the
# file there, so it wins here too.
edge_port() {
    if [ -n "${PRO_SERVER__PORT:-}" ]; then echo "$PRO_SERVER__PORT"; return; fi
    local cfg=$PRO_DIR/pro.json
    [ -f "$cfg" ] || { echo 8080; return; }
    # Parsed rather than grepped: pro.json is operator-edited and its
    # formatting is not ours to assume.
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$cfg" <<'PY' 2>/dev/null || echo 8080
import json, sys
try:
    c = json.load(open(sys.argv[1]))
except Exception:
    print(8080); raise SystemExit
port = (c.get("server") or {}).get("port")
if port:
    print(port)
else:
    print(443 if (c.get("ssl") or {}).get("enabled") else 8080)
PY
    else
        echo 8080
    fi
}

# The platform's own address, as it tells customers. Read from the config
# where there is one, otherwise assembled from the port — this is only used in
# the messages printed here, so a guess costs nothing.
config_url() {
    local url="" scheme=http
    if command -v python3 >/dev/null 2>&1 && [ -f "$PRO_DIR/pro.json" ]; then
        url=$(python3 -c 'import json,sys
try: c=json.load(open(sys.argv[1]))
except Exception: raise SystemExit(1)
print(c.get("url") or "")' "$PRO_DIR/pro.json" 2>/dev/null || true)
        scheme=$(python3 -c 'import json,sys
try: c=json.load(open(sys.argv[1]))
except Exception: print("http"); raise SystemExit
print("https" if (c.get("ssl") or {}).get("enabled") else "http")' "$PRO_DIR/pro.json" 2>/dev/null || echo http)
    fi
    if [ -n "$url" ]; then echo "$url"; else echo "$scheme://localhost:$(edge_port)"; fi
}

PORT=$(edge_port)
URL=$(config_url)

# ---------------------------------------------------------------------------
# Is it ours, and is it alive
# ---------------------------------------------------------------------------

pid() { [ -f "$PIDFILE" ] && cat "$PIDFILE" 2>/dev/null || true; }

# Pids are reused, and a pidfile that outlives its process can otherwise name
# something else entirely.
is_pro() { ps -o args= -p "$1" 2>/dev/null | grep -q "ghost-pro"; }

alive() {
    local p
    p=$(pid)
    [ -n "$p" ] && kill -0 "$p" 2>/dev/null && is_pro "$p"
}

# Whatever is listening on the edge port, started by this script or not.
listener() { lsof -ti "tcp:$PORT" -sTCP:LISTEN 2>/dev/null | head -1; }

# The same, but only if it is this platform. A start that fails because
# something else owns the port is a different problem from a stale instance of
# our own, and only one of the two is ours to kill.
pro_listener() {
    local p
    p=$(listener)
    if [ -n "$p" ] && is_pro "$p"; then echo "$p"; fi
}

describe_listener() {
    local p
    p=$(listener)
    [ -n "$p" ] && ps -o pid=,args= -p "$p" 2>/dev/null | sed 's/^ *//'
}

# Tenant sites currently listening. After a clean stop this is empty; anything
# here with the platform down is an orphan holding a tenant's database open.
tenant_pids() { lsof -ti "tcp:$TENANT_PORTS" -sTCP:LISTEN 2>/dev/null | sort -u || true; }

tenant_count() { tenant_pids | grep -c . || true; }

# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Bootstrap
# ---------------------------------------------------------------------------
#
# The deployment directory holds only generated and runtime state — the
# database, tenant site directories, certificates, backups, logs — so it is
# not in the repository, the same way Vuuut's content/data and
# content/images are not in its own. The difference that used to matter is
# that Vuuut's `run.sh` works on a fresh clone because the server creates
# what it is missing, while this one refused to start without a `pro.json`
# nobody had written yet. So it writes one: a local-development config, for
# the layout a clone actually lands in. `init` does it on request, `start`
# does it on demand, and either is safe to run twice.
init_dir() {
    mkdir -p "$PRO_DIR"
    if [ -f "$PRO_DIR/pro.json" ]; then
        return 0
    fi
    require_bin
    echo "no pro.json in $PRO_DIR — writing one for local development…"
    # Local defaults, matching Vuuut's: a hostname that needs no DNS and
    # no DNS entry, and plain HTTP. The paths to Vuuut are found by the
    # setup command and written relative to this directory, so the config does
    # not name this machine. Everything here is editable afterwards.
    #
    # </dev/null because setup prompts when it has a terminal, and a bootstrap
    # that stops to ask questions is not a bootstrap.
    (cd "$PRO_DIR" && "$BIN" setup \
        --url "${PRO_URL:-http://pro.localhost:8080}" \
        --subdomain-base "${PRO_SUBDOMAIN_BASE:-localhost}" \
        </dev/null)
}

require_dir() {
    if [ ! -d "$PRO_DIR" ] || [ ! -f "$PRO_DIR/pro.json" ]; then
        init_dir
        # The port and URL were read before this existed, so read them again.
        PORT=$(edge_port)
        URL=$(config_url)
    fi
}

require_bin() {
    if [ ! -x "$BIN" ]; then
        echo "no binary — building first…"
        cargo build --release
    fi
}

# Build the tenant executable too when its configured path names a local CMS
# checkout. Resolve the same file/environment layers as the platform first.
require_tenant_bin() {
    command -v python3 >/dev/null 2>&1 || return 0
    local tenant_bin checkout
    tenant_bin=$(python3 - "$PRO_DIR" <<'PYTHON'
import json, os, pathlib, shutil, sys
base = pathlib.Path(sys.argv[1]).resolve()
env = os.environ.get("PRO_ENV", os.environ.get("NODE_ENV", "development"))
config = {"paths": {"ghostBinary": "ghost"}}
def merge(dst, src):
    if isinstance(dst, dict) and isinstance(src, dict):
        for key, value in src.items():
            dst[key] = merge(dst.get(key), value)
        return dst
    return src
try:
    for filename in ("pro.json", "pro." + env + ".json"):
        path = base / filename
        if path.exists():
            config = merge(config, json.loads(path.read_text()))
    for key, value in os.environ.items():
        if not key.startswith("PRO_") or key == "PRO_ENV":
            continue
        def camel(part):
            first, *rest = part.lower().split("_")
            return first + "".join(word.capitalize() for word in rest)
        parts = [camel(part) for part in key[4:].split("__")]
        if not all(parts):
            continue
        current = config
        for part in parts[:-1]:
            if not isinstance(current.get(part), dict):
                current[part] = {}
            current = current[part]
        try:
            parsed = json.loads(value)
        except ValueError:
            parsed = value
        current[parts[-1]] = parsed
    binary = config["paths"]["ghostBinary"]
    if not isinstance(binary, str) or not binary:
        raise ValueError("paths.ghostBinary must be a nonempty string")
    print((base / binary).resolve() if "/" in binary else (shutil.which(binary) or binary))
except (OSError, ValueError, TypeError, KeyError) as error:
    print("cannot resolve tenant binary: " + str(error), file=sys.stderr)
    sys.exit(1)
PYTHON
    )
    if [ -x "$tenant_bin" ] && [ -f "$tenant_bin" ]; then return 0; fi
    if [[ "$tenant_bin" = */target/release/ghost ]]; then
        checkout=${tenant_bin%/target/release/ghost}
        if [ -f "$checkout/Cargo.toml" ] && [ -f "$checkout/crates/ghost/Cargo.toml" ]; then
            echo "no tenant CMS binary — building Vuuut in ${checkout}…"
            cargo build --release --manifest-path "$checkout/Cargo.toml" \
                --target-dir "$checkout/target" --bin ghost
        fi
    fi
    if [ ! -f "$tenant_bin" ] || [ ! -x "$tenant_bin" ]; then
        echo "tenant CMS binary is missing or not executable: $tenant_bin" >&2
        echo "build Vuuut with cargo build --release, or correct paths.ghostBinary in $PRO_DIR/pro.json." >&2
        return 1
    fi
}

case "${1:-start}" in
# Make the deployment directory and its config, without starting anything.
# What `start` does for itself, for an operator who wants to read the config
# over before the platform runs with it.
init)
    if [ -f "$PRO_DIR/pro.json" ]; then
        echo "already initialised: $PRO_DIR/pro.json"
        echo "delete it to start over, or edit it in place."
        exit 0
    fi
    init_dir
    ;;

start)
    require_dir
    if alive; then echo "already running (pid $(pid)) at $URL"; exit 0; fi
    rm -f "$PIDFILE"

    # An instance whose pidfile was lost still serves the platform, and
    # starting a second one on the same port only produces a process that
    # exits a moment later. Adopt it instead, so stop/restart can reach it.
    existing=$(pro_listener)
    if [ -n "$existing" ]; then
        echo "$existing" >"$PIDFILE"
        echo "already running (pid $existing, adopted) at $URL"
        exit 0
    fi
    if [ -n "$(listener)" ]; then
        echo "port $PORT is held by something that is not this platform:"
        echo "  $(describe_listener)"
        echo "stop it, or set PRO_SERVER__PORT to a free port."
        exit 1
    fi

    require_bin
    require_tenant_bin
    log_start=1
    if [ -f "$LOG" ]; then log_start=$(( $(wc -c <"$LOG") + 1 )); fi
    cd "$PRO_DIR"
    if command -v python3 >/dev/null 2>&1; then
        # A new session also survives a launching automation's process group
        # ending; nohup alone only ignores SIGHUP.
        started=$(python3 - "$BIN" "$LOG" <<'PYTHON'
import subprocess, sys
with open(sys.argv[2], "ab", buffering=0) as log:
    child = subprocess.Popen([sys.argv[1], "serve"], stdin=subprocess.DEVNULL,
                             stdout=log, stderr=subprocess.STDOUT,
                             start_new_session=True, close_fds=True)
print(child.pid)
PYTHON
        )
    else
        nohup "$BIN" serve </dev/null >>"$LOG" 2>&1 &
        started=$!
    fi
    echo "$started" >"$PIDFILE"
    cd "$ROOT"

    # Wait for it to serve rather than assuming it will. Most startup failures
    # — a busy port, a migration that will not apply, a bad config — happen
    # within the first second and leave nothing behind but the log.
    for _ in $(seq 1 100); do
        if ! kill -0 "$started" 2>/dev/null; then
            rm -f "$PIDFILE"
            echo "failed to start — output from this launch ($LOG):"
            tail -c +"$log_start" "$LOG" | tail -n 20 | sed 's/^/  /'
            exit 1
        fi
        [ "$(listener)" = "$started" ] && break
        sleep 0.1
    done

    if [ "$(listener)" = "$started" ]; then
        echo "started (pid $started)"
        echo "  dashboard  $URL/dashboard"
        echo "  console    $URL/console"
        echo "  log        $LOG"
    else
        # Running, but nothing on the port yet. Worth saying plainly: the
        # process being up is not the same as the platform being up.
        echo "started (pid $started) but nothing is listening on $PORT yet — check $LOG"
    fi
    ;;

stop)
    if alive; then
        target=$(pid)
    else
        # The pidfile is stale or gone; fall back to the port, so an instance
        # this script did not start can still be stopped.
        target=$(pro_listener)
    fi
    rm -f "$PIDFILE"
    if [ -z "$target" ]; then
        if [ -n "$(listener)" ]; then
            echo "not running — but port $PORT is held by:"
            echo "  $(describe_listener)"
        else
            echo "not running"
        fi
        orphans=$(tenant_count)
        [ "$orphans" -gt 0 ] && echo "note: $orphans tenant site(s) still running — ./run.sh reap"
        exit 0
    fi

    running_before=$(tenant_count)
    kill -TERM "$target" 2>/dev/null || true

    # Generous: on the way out the platform stops every tenant site in turn so
    # each closes its database cleanly, and a machine holding a few hundred
    # sites takes longer than a web server would.
    for _ in $(seq 1 300); do
        kill -0 "$target" 2>/dev/null || break
        sleep 0.1
    done

    if kill -0 "$target" 2>/dev/null; then
        # Thirty seconds of ignoring SIGTERM is not a shutdown in progress.
        # Forcing it skips the tenant shutdown, so say what that leaves.
        kill -9 "$target" 2>/dev/null || true
        echo "stopped (pid $target, forced after 30s)"
        left=$(tenant_count)
        if [ "$left" -gt 0 ]; then
            echo "warning: $left tenant site(s) were not stopped and still hold their databases."
            echo "         clear them with: ./run.sh reap"
        fi
    else
        echo "stopped (pid $target)"
        left=$(tenant_count)
        if [ "$left" -gt 0 ]; then
            echo "warning: $left of $running_before tenant site(s) outlived the platform — ./run.sh reap"
        fi
    fi
    ;;

restart) "$0" stop || true; "$0" start ;;

status)
    # Said before anything is probed. Without a config there is no port to
    # probe either, so the fallback would be a guess — and a guess that lands
    # on another deployment's port reports *that* one as this one's.
    if [ ! -f "$PRO_DIR/pro.json" ]; then
        echo "no deployment at $PRO_DIR (no pro.json)"
        [ -d "$PRO_DIR" ] || echo "  the directory does not exist either"
        echo "  create one:  ./run.sh init    (or just ./run.sh start)"
        exit 0
    fi
    if alive; then
        echo "running (pid $(pid)) at $URL"
    elif [ -n "$(pro_listener)" ]; then
        echo "running (pid $(pro_listener)) at $URL — started outside this script"
    elif [ -n "$(listener)" ]; then
        echo "not running — port $PORT is held by:"
        echo "  $(describe_listener)"
    else
        echo "not running"
    fi

    sites=$(tenant_count)
    echo "tenant sites awake: $sites"

    # Only meaningful while it is up, and the cheapest real check there is:
    # the platform reports its own database reachability here.
    #
    # Asked over loopback with the platform's hostname in the Host header,
    # not by resolving that hostname: in production it points at whatever is
    # in front of this machine, and the question here is whether *this*
    # process is healthy. The header matters because the edge decides what a
    # request is from its Host — anything else is treated as a tenant's and
    # answered with "no site here".
    if [ -n "$(pro_listener)" ] && command -v curl >/dev/null 2>&1; then
        host=${URL#*://}
        host=${host%%/*}
        health=$(curl -fsS --max-time 3 -H "Host: $host" \
                      "http://127.0.0.1:$PORT/health" 2>/dev/null || true)
        if [ -n "$health" ]; then
            echo "health: $health"
        else
            echo "health: no answer on 127.0.0.1:$PORT as Host $host"
        fi
    fi
    echo "config: $PRO_DIR/pro.json"
    ;;

# Everything a tenant process is, as the platform sees it. Useful when a site
# is slow and the question is whether it is even awake.
tenants)
    pids=$(tenant_pids)
    if [ -z "$pids" ]; then echo "no tenant sites are running"; exit 0; fi
    printf "%-8s %-7s %-22s %s\n" PID PORT SITE ID
    for p in $pids; do
        # `-a` is load-bearing: without it lsof treats -p and -i as a union
        # and reports every listening socket on the machine. `-Fn` gives one
        # `n<addr>:<port>` line, which beats counting columns.
        port=$(lsof -a -p "$p" -iTCP -sTCP:LISTEN -Fn 2>/dev/null \
               | sed -n 's/^n.*:\([0-9]\{1,\}\)$/\1/p' | head -1)
        # A tenant runs with its own directory as the working directory, and
        # that directory is named for its id.
        dir=$(lsof -a -p "$p" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1)
        id=$(basename "${dir:-?}")
        # The slug is what an operator recognises; the id is what the console
        # links to. Both, when the database can be read.
        slug=""
        if command -v sqlite3 >/dev/null 2>&1 && [ -f "$PRO_DIR/data/pro.db" ]; then
            slug=$(sqlite3 "$PRO_DIR/data/pro.db" \
                   "select slug from tenants where id='$id';" 2>/dev/null || true)
        fi
        printf "%-8s %-7s %-22s %s\n" "$p" "${port:-?}" "${slug:-?}" "$id"
    done
    ;;

# Stop tenant sites the platform left behind. Only ever needed after a forced
# stop or a crash — a clean shutdown takes them with it. SIGTERM first, so each
# still closes its database properly.
reap)
    pids=$(tenant_pids)
    if [ -z "$pids" ]; then echo "nothing to reap"; exit 0; fi
    if [ -n "$(pro_listener)" ]; then
        echo "the platform is running — these sites are its children, not orphans."
        echo "stop it first (./run.sh stop), which stops them too."
        exit 1
    fi
    n=$(echo "$pids" | grep -c .)
    echo "stopping $n orphaned tenant site(s)…"
    for p in $pids; do kill -TERM "$p" 2>/dev/null || true; done
    for _ in $(seq 1 100); do
        [ -z "$(tenant_pids)" ] && break
        sleep 0.1
    done
    left=$(tenant_pids)
    if [ -n "$left" ]; then
        for p in $left; do kill -9 "$p" 2>/dev/null || true; done
        echo "reaped (some forced)"
    else
        echo "reaped"
    fi
    ;;

build) cargo build --release ;;
rebuild) cargo clean && cargo build --release ;;
test) cargo test --workspace ;;
log) tail -f "$LOG" ;;

# The platform's own preflight: config, binary, themes, database, ports, mail,
# billing. Run from the deployment directory, which is what it inspects.
doctor)
    require_dir; require_bin
    cd "$PRO_DIR" && exec "$BIN" doctor
    ;;

# An operator login. Without one the console cannot be opened at all, and the
# command line is the only way in.
staff)
    require_dir; require_bin
    shift || true
    cd "$PRO_DIR" && exec "$BIN" create-staff "$@"
    ;;

*)
    cat <<EOF
usage: ./run.sh <command>

  init       create the deployment directory and its pro.json
  start      start the platform (init first if needed; adopts an instance
             already on the port)
  stop       stop it, and every tenant site with it
  restart    stop then start
  status     is it up, how many sites are awake, what it says about itself
  log        follow the log
  doctor     check this machine can serve the current config
  tenants    list the tenant sites currently running
  reap       stop tenant sites left behind by a crash or a forced stop
  staff      create an operator login, e.g. ./run.sh staff --email you@x.com --name You
  build      cargo build --release
  rebuild    cargo clean && cargo build --release
  test       cargo test --workspace

environment:
  PRO_DIR               deployment directory (default: ./run)
  PRO_SERVER__PORT      override the edge port from pro.json
  PRO_URL               platform URL written by init
                        (default: http://pro.localhost:8080)
  PRO_SUBDOMAIN_BASE    tenant zone written by init (default: localhost)
EOF
    exit 1
    ;;
esac
