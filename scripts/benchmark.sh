#!/usr/bin/env bash
# Benchmark omavrything search latency and indexing throughput.
#
# Measures, over the real Unix-socket protocol:
#   * cold-scan indexing throughput (entries/second),
#   * search latency on an idle index (selective, broad, no-match),
#   * search latency while the index is being rebuilt (a rescan), and
#   * search latency while the index is being updated live (an inotify burst),
# plus, against the live installed daemon when one is reachable.
# Optionally runs the Criterion micro-benchmark and the standalone profiler
# example (both need a source checkout).
#
# Usage: scripts/benchmark.sh [options]
#
# Options:
#   --live              Benchmark the running/installed daemon only
#   --hermetic          Benchmark a private daemon on a synthetic tree only
#   --rescan            Also measure search latency during a periodic rescan
#                       (idle-gated; usually only fires in an idle session or CI)
#   --no-live-update    Skip the inotify live-update measurement
#   --criterion         Also run the Criterion micro-benchmark (cargo bench)
#   --profile           Also run the standalone profiler example
#   --full              Larger sizes and the rescan measurement
#   --all               --full plus the Criterion bench and the profiler
#   --quick             Small sizes for a fast run (CI)
#   --files N           Synthetic tree size (default 5000)
#   --queries N         Queries per hermetic latency measurement (default 30)
#   --live-queries N    Queries per live-daemon measurement (default 3)
#   --duration S        Seconds to sample during indexing (default 4)
#   --records N         Synthetic in-memory index size for --profile (default 1000000)
#   --socket PATH       Use a specific socket for the live daemon
#   --json              Emit a single machine-readable JSON object on stdout
#   --keep              Keep the temporary tree and logs
#   -h, --help          Show this help
#
# The default runs the hermetic benchmark and, if a daemon is reachable, the
# live benchmark. Human-readable progress goes to stderr; with --json the
# machine-readable summary is the only thing on stdout. Exit status is 0 when
# every requested phase ran; a phase that could not run (no daemon, no source
# checkout) is reported and skipped.
#
# Requires: bash, python3, and the omavrythingd binary (bin/ or /usr/bin).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

run_live=auto
run_hermetic=1
do_indexing=0
do_live_update=1
do_criterion=0
do_profile=0
json=0
keep=0
files=5000
queries=30
duration=4
live_queries=3
records=1000000
socket_override=""

usage() { sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --live) run_live=yes; run_hermetic=0 ;;
    --hermetic) run_live=no; run_hermetic=1 ;;
    --rescan) do_indexing=1 ;;
    --no-live-update) do_live_update=0 ;;
    --criterion) do_criterion=1 ;;
    --profile) do_profile=1 ;;
    --full) files=50000; queries=100; duration=15; live_queries=20; do_indexing=1 ;;
    --all) files=50000; queries=100; duration=15; live_queries=20; do_indexing=1; do_criterion=1; do_profile=1 ;;
    --quick) files=2000; queries=20; duration=3; live_queries=2; records=200000 ;;
    --files) files="$2"; shift ;;
    --queries) queries="$2"; shift ;;
    --live-queries) live_queries="$2"; shift ;;
    --duration) duration="$2"; shift ;;
    --records) records="$2"; shift ;;
    --socket) socket_override="$2"; shift ;;
    --json) json=1 ;;
    --keep) keep=1 ;;
    -h | --help) usage; exit 0 ;;
    *)
      echo "error: unknown option: $1" >&2
      exit 2
      ;;
  esac
  shift
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required" >&2
  exit 1
fi

# ---- output helpers --------------------------------------------------------
# Human-readable progress goes to stderr so stdout stays clean for --json.
say() { printf '%s\n' "$*" >&2; }
section() { printf '\n== %s ==\n' "$*" >&2; }

tmp="$(mktemp -d)"
chmod 755 "$tmp"
results="$tmp/results.tsv"
: > "$results"
py="$tmp/bench.py"
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  if [[ "$keep" == 1 ]]; then
    say "keeping $tmp (--keep)"
  else
    rm -rf "$tmp"
  fi
}
trap cleanup EXIT

add_result() { printf '%s\t%s\n' "$1" "$2" >> "$results"; }

# ---- locate the binaries ---------------------------------------------------
daemon_bin=""
for cand in "$here/bin/omavrythingd" /usr/bin/omavrythingd; do
  if [[ -x "$cand" ]]; then
    daemon_bin="$cand"
    break
  fi
done
cli_bin=""
for cand in "$here/bin/omavry" /usr/bin/omavry; do
  if [[ -x "$cand" ]]; then
    cli_bin="$cand"
    break
  fi
done

if [[ -z "$daemon_bin" ]]; then
  say "error: omavrythingd not found (run scripts/build.sh or install it)" >&2
  exit 1
fi

# ---- the embedded socket client -------------------------------------------
# Speaks the newline-delimited JSON protocol directly so a measurement is not
# polluted by per-query process startup. Wall-clock round-trip is the primary
# metric (the daemon's took_ms is integer milliseconds and rounds sub-ms
# queries to 0); both are reported.
cat > "$py" <<'OMAVRYTHING_BENCH_PY'
#!/usr/bin/env python3
"""Socket client and statistics for scripts/benchmark.sh."""
import argparse
import json
import os
import socket
import statistics
import time


def connect(path):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(30)
    s.connect(path)
    return s


def do_query(sock, q, limit, offset):
    req = json.dumps({"q": q, "limit": limit, "offset": offset}) + "\n"
    sock.sendall(req.encode("utf-8"))
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = sock.recv(65536)
        if not chunk:
            raise RuntimeError("daemon closed the connection")
        buf += chunk
    return json.loads(buf.decode("utf-8"))


def percentile(sorted_xs, p):
    n = len(sorted_xs)
    if n == 0:
        return 0.0
    k = int(round((p / 100.0) * (n - 1)))
    return sorted_xs[max(0, min(n - 1, k))]


def summarize(wall_ms, took_ms):
    if not wall_ms:
        return None
    ws = sorted(wall_ms)
    ts = sorted(took_ms)
    return {
        "count": len(ws),
        "wall_ms": {
            "min": round(ws[0], 3),
            "p50": round(percentile(ws, 50), 3),
            "p90": round(percentile(ws, 90), 3),
            "p95": round(percentile(ws, 95), 3),
            "p99": round(percentile(ws, 99), 3),
            "max": round(ws[-1], 3),
            "mean": round(statistics.fmean(ws), 3),
        },
        "took_ms": {
            "min": ts[0],
            "p50": percentile(ts, 50),
            "p90": percentile(ts, 90),
            "p95": percentile(ts, 95),
            "p99": percentile(ts, 99),
            "max": ts[-1],
            "mean": round(statistics.fmean(ts), 3),
        },
    }


def emit_latency(title, s):
    if not s:
        print(f"  {title}: no samples")
        return
    w, t = s["wall_ms"], s["took_ms"]
    print(
        f"  {title}: n={s['count']}  "
        f"wall p50={w['p50']}ms p95={w['p95']}ms p99={w['p99']}ms "
        f"max={w['max']}ms mean={w['mean']}ms  |  "
        f"daemon took_ms p50={t['p50']} p95={t['p95']} max={t['max']}"
    )


def cmd_latency(a):
    sock = connect(a.socket)
    for _ in range(a.warmup):
        do_query(sock, a.query, a.limit, 0)
    wall, took = [], []
    for _ in range(a.count):
        t0 = time.perf_counter()
        resp = do_query(sock, a.query, a.limit, 0)
        wall.append((time.perf_counter() - t0) * 1e3)
        took.append(int(resp.get("took_ms", 0)))
    sock.close()
    s = summarize(wall, took)
    if a.json:
        print(json.dumps(s))
    else:
        emit_latency(a.title or a.query, s)


def cmd_record(a):
    sock = connect(a.socket)
    logf = None
    if a.log:
        logf = open(a.log, "r", errors="replace")
        logf.seek(0, os.SEEK_END)
    out = open(a.out, "w")
    windows = open(a.windows_out, "w") if a.windows_out else None
    start = time.monotonic()
    rescan_start = None
    while time.monotonic() - start < a.duration:
        t0 = time.perf_counter()
        resp = do_query(sock, a.query, a.limit, 0)
        dt = (time.perf_counter() - t0) * 1e3
        now_ms = time.time() * 1e3
        out.write(f"{now_ms:.3f} {dt:.3f} {int(resp.get('took_ms', 0))}\n")
        out.flush()
        if logf is not None:
            while True:
                line = logf.readline()
                if not line:
                    break
                if "rescanning roots" in line:
                    rescan_start = time.time() * 1e3
                elif "rescan complete" in line and rescan_start is not None:
                    if windows is not None:
                        windows.write(f"{rescan_start:.3f} {time.time() * 1e3:.3f}\n")
                        windows.flush()
                    rescan_start = None
    sock.close()
    out.close()
    if windows is not None:
        windows.close()


def cmd_report(a):
    samples = []
    with open(a.samples) as f:
        for line in f:
            parts = line.split()
            if len(parts) >= 3:
                samples.append((float(parts[0]), float(parts[1]), int(parts[2])))
    windows = []
    if a.windows and os.path.exists(a.windows):
        with open(a.windows) as f:
            for line in f:
                parts = line.split()
                if len(parts) >= 2:
                    windows.append((float(parts[0]), float(parts[1])))

    def in_window(t):
        return any(s <= t <= e for s, e in windows)

    idle = [s for s in samples if not in_window(s[0])]
    during = [s for s in samples if in_window(s[0])]
    result = {
        "total": len(samples),
        "windows": len(windows),
        "idle": summarize([s[1] for s in idle], [s[2] for s in idle]),
        "during": summarize([s[1] for s in during], [s[2] for s in during]),
    }
    if a.json:
        print(json.dumps(result))
    else:
        print(f"  windows observed: {result['windows']}  samples: {result['total']}")
        if result["windows"] == 0:
            print(
                "  note: no indexing window was observed. The periodic rescan is "
                "idle-gated (load, CPU idle, and the login1 idle hint), so an "
                "active desktop session vetoes it; it runs in an idle session or "
                "in CI. The live-update burst is not gated."
            )
        emit_latency("idle  ", result["idle"])
        emit_latency("during", result["during"])


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)

    lat = sub.add_parser("latency")
    lat.add_argument("--socket", required=True)
    lat.add_argument("--query", required=True)
    lat.add_argument("--count", type=int, default=200)
    lat.add_argument("--warmup", type=int, default=20)
    lat.add_argument("--limit", type=int, default=100)
    lat.add_argument("--title", default="")
    lat.add_argument("--json", action="store_true")
    lat.set_defaults(func=cmd_latency)

    rec = sub.add_parser("record")
    rec.add_argument("--socket", required=True)
    rec.add_argument("--query", required=True)
    rec.add_argument("--duration", type=float, default=25)
    rec.add_argument("--limit", type=int, default=100)
    rec.add_argument("--out", required=True)
    rec.add_argument("--log", default="")
    rec.add_argument("--windows-out", default="")
    rec.set_defaults(func=cmd_record)

    rep = sub.add_parser("report")
    rep.add_argument("--samples", required=True)
    rep.add_argument("--windows", default="")
    rep.add_argument("--json", action="store_true")
    rep.set_defaults(func=cmd_report)

    a = p.parse_args()
    a.func(a)


if __name__ == "__main__":
    main()
OMAVRYTHING_BENCH_PY

# ---- environment report ----------------------------------------------------
version="$("$daemon_bin" --version 2>/dev/null || echo unknown)"
cpus="$(nproc 2>/dev/null || echo '?')"
section "environment"
say "version: $version"
say "cpu:     $cpus cores"
say "daemon:  $daemon_bin"
say "cli:     ${cli_bin:-not found}"
if [[ "$json" == 1 ]]; then
  add_result "environment" "$(python3 -c 'import json,sys; print(json.dumps({"version": sys.argv[1], "cpus": sys.argv[2], "daemon": sys.argv[3]}))' "$version" "$cpus" "$daemon_bin")"
fi

# ---- latency measurement helper -------------------------------------------
measure_latency() {
  local title="$1" query="$2" sock="$3" count="${4:-$queries}"
  # Scale the warmup to the sample count: a live query on a multi-million-entry
  # index costs hundreds of milliseconds, so a fixed 20-query warmup would
  # dominate the run.
  local warmup=$((count < 20 ? count : 20))
  if [[ "$json" == 1 ]]; then
    local out
    out="$(python3 "$py" latency --socket "$sock" --query "$query" --count "$count" --warmup "$warmup" --json)"
    add_result "latency:$title" "$out"
  else
    python3 "$py" latency --socket "$sock" --query "$query" --count "$count" --warmup "$warmup" --title "$title" >&2
  fi
}

# ---- live daemon -----------------------------------------------------------
live_socket=""
if [[ -n "$socket_override" ]]; then
  live_socket="$socket_override"
elif [[ -S /run/omavrything/omavrything.sock ]]; then
  live_socket=/run/omavrything/omavrything.sock
elif [[ -n "${XDG_RUNTIME_DIR:-}" && -S "$XDG_RUNTIME_DIR/omavrything.sock" ]]; then
  live_socket="$XDG_RUNTIME_DIR/omavrything.sock"
fi

if [[ "$run_live" == "no" ]]; then
  :
elif [[ -n "$live_socket" ]]; then
  run_live=yes
else
  run_live=no
fi

if [[ "$run_live" == "yes" ]]; then
  section "live daemon ($live_socket)"
  live_entries=""
  if [[ -n "$cli_bin" ]]; then
    live_entries="$("$cli_bin" status 2>/dev/null | sed -n 's/^entries: //p' || true)"
  fi
  if [[ -n "$live_entries" ]]; then
    say "index entries: $live_entries"
    if [[ "$json" == 1 ]]; then
      add_result "live" "$(python3 -c 'import json,sys; print(json.dumps({"socket": sys.argv[1], "entries": int(sys.argv[2])}))' "$live_socket" "$live_entries")"
    fi
  fi
  # The live index is large (millions of entries) and a broad query scans all of
  # it, so the live phase uses a smaller sample count than the hermetic one.
  for q in "a" "config" "zzz-no-such-file-zzzz"; do
    measure_latency "live \"$q\"" "$q" "$live_socket" "$live_queries"
  done
fi

# ---- hermetic daemon -------------------------------------------------------
if [[ "$run_hermetic" == 1 ]]; then
  section "hermetic daemon"
  root="$tmp/root"
  run="$tmp/run"
  mkdir -p "$root" "$run"
  say "creating a synthetic tree of $files files..."
  python3 - "$root" "$files" <<'PY'
import os
import sys

root, n = sys.argv[1], int(sys.argv[2])
dirs = 1000
for d in range(dirs):
    os.makedirs(os.path.join(root, f"dir{d:04d}", f"sub{d % 50:02d}"), exist_ok=True)
for i in range(n):
    d = i % dirs
    p = os.path.join(root, f"dir{d:04d}", f"sub{d % 50:02d}", f"file-{i:07d}.txt")
    open(p, "w").close()
PY
  say "tree ready"

  sock="$run/omavrything.sock"
  log="$tmp/daemon.log"

  start_daemon() {
    local extra=("$@")
    : > "$log"
    # A previous run leaves its socket file behind; remove it so the wait below
    # cannot be satisfied by a stale, unlistened socket.
    rm -f "$sock"
    "$daemon_bin" --no-config --root "$root" --socket "$sock" \
      --no-dbus --no-snapshot --no-ntfs --log-level info "${extra[@]}" \
      >"$log" 2>&1 &
    daemon_pid=$!
    for _ in $(seq 1 600); do
      [[ -S "$sock" ]] && return 0
      kill -0 "$daemon_pid" 2>/dev/null || return 1
      sleep 0.1
    done
    return 1
  }

  stop_daemon() {
    if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
      kill "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
    fi
    daemon_pid=""
  }

  # --- run A: cold scan + idle latency + during-rescan ---------------------
  if start_daemon --scan-interval 1; then
    scan_line="$(grep -m1 'indexed entries=' "$log" || true)"
    entries="$(sed -n 's/.*entries=\([0-9]*\).*/\1/p' <<<"$scan_line")"
    elapsed="$(sed -n 's/.*elapsed_ms=\([0-9]*\).*/\1/p' <<<"$scan_line")"
    if [[ -n "$entries" && -n "$elapsed" && "$elapsed" -gt 0 ]]; then
      eps="$(python3 -c "print(round($entries / ($elapsed / 1000.0), 1))")"
      say "cold scan: $entries entries in ${elapsed} ms ($eps entries/s)"
      if [[ "$json" == 1 ]]; then
        add_result "indexing" "{\"entries\": $entries, \"elapsed_ms\": $elapsed, \"entries_per_sec\": $eps}"
      fi
    else
      say "cold scan: could not parse the scan line"
    fi

    say "idle search latency:"
    measure_latency "selective \"file-0001234\"" "file-0001234" "$sock"
    measure_latency "broad \"dir0001\"" "dir0001" "$sock"
    measure_latency "no-match \"zzz-no-such-file-zzzz\"" "zzz-no-such-file-zzzz" "$sock"

    if [[ "$do_indexing" == 1 ]]; then
      say "search latency during a periodic rescan (idle-gated), ${duration}s:"
      python3 "$py" record --socket "$sock" --query "file-0001234" \
        --duration "$duration" --out "$tmp/samples.txt" \
        --log "$log" --windows-out "$tmp/windows.txt" &
      rec_pid=$!
      wait "$rec_pid" || true
      if [[ "$json" == 1 ]]; then
        out="$(python3 "$py" report --samples "$tmp/samples.txt" --windows "$tmp/windows.txt" --json)"
        add_result "during_rescan" "$out"
      else
        python3 "$py" report --samples "$tmp/samples.txt" --windows "$tmp/windows.txt" >&2
      fi
    fi
    stop_daemon
  else
    say "error: the hermetic daemon did not start; see $log" >&2
    sed 's/^/  /' "$log" >&2 || true
  fi

  # --- run B: live-update burst --------------------------------------------
  if [[ "$do_live_update" == 1 ]]; then
    if start_daemon --scan-interval 0; then
      burst_files=$((files / 5))
      [[ "$burst_files" -lt 1000 ]] && burst_files=1000
      burst_duration=$duration
      say "search latency during live index updates (inotify burst of $burst_files files), ${burst_duration}s:"
      python3 "$py" record --socket "$sock" --query "file-0001234" \
        --duration "$burst_duration" --out "$tmp/burst-samples.txt" &
      rec_pid=$!
      sleep 2
      burst_start="$(python3 -c 'import time; print(time.time() * 1e3)')"
      python3 - "$root" "$burst_files" <<'PY' &
import os
import sys

root, n = sys.argv[1], int(sys.argv[2])
for i in range(n):
    d = i % 1000
    p = os.path.join(root, f"dir{d:04d}", f"sub{d % 50:02d}", f"burst-{i:07d}.txt")
    open(p, "w").close()
PY
      burst_pid=$!
      wait "$burst_pid" || true
      # Extend the window by 1 s so the watcher's 500 ms poll has time to apply
      # the last batch of events.
      burst_end="$(python3 -c 'import time; print(time.time() * 1e3 + 1000)')"
      printf '%s %s\n' "$burst_start" "$burst_end" > "$tmp/burst-windows.txt"
      wait "$rec_pid" || true
      if [[ "$json" == 1 ]]; then
        out="$(python3 "$py" report --samples "$tmp/burst-samples.txt" --windows "$tmp/burst-windows.txt" --json)"
        add_result "during_live_update" "$out"
      else
        python3 "$py" report --samples "$tmp/burst-samples.txt" --windows "$tmp/burst-windows.txt" >&2
      fi
      stop_daemon
    else
      say "error: the hermetic daemon did not start for the live-update run" >&2
    fi
  fi
fi

# ---- optional: Criterion micro-benchmark ----------------------------------
if [[ "$do_criterion" == 1 ]]; then
  section "criterion micro-benchmark"
  if [[ -f Cargo.toml ]]; then
    cargo bench -p omavrything-core --bench search >&2
  else
    say "skip: --criterion needs a source checkout (no Cargo.toml)"
  fi
fi

# ---- optional: standalone profiler ----------------------------------------
if [[ "$do_profile" == 1 ]]; then
  section "standalone profiler ($records records)"
  if [[ -f Cargo.toml ]]; then
    cargo run --release -p omavrything-core --example profile -- "$records" report >&2
  else
    say "skip: --profile needs a source checkout (no Cargo.toml)"
  fi
fi

# ---- summary ---------------------------------------------------------------
if [[ "$json" == 1 ]]; then
  python3 - "$results" <<'PY'
import json
import sys

out = {}
with open(sys.argv[1]) as f:
    for line in f:
        line = line.rstrip("\n")
        if not line:
            continue
        key, _, val = line.partition("\t")
        try:
            out[key] = json.loads(val)
        except Exception:
            out[key] = val
print(json.dumps(out, indent=2, sort_keys=True))
PY
else
  section "done"
  say "benchmark complete"
fi
