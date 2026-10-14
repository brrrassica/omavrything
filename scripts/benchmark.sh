#!/usr/bin/env bash
# Benchmark omavrything search latency and indexing throughput.
#
# Measures, over the real Unix-socket protocol:
#   * cold-scan indexing throughput (entries/second),
#   * search latency on an idle index (selective, broad, no-match),
#   * search latency while the index is being rebuilt (a rescan),
#   * search latency while the index is being updated live (an inotify burst),
#   * the daemon's resident set size (RSS) and peak RSS (VmHWM), and
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
#   --no-memory         Skip the daemon RSS/peak-memory measurement
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
#   --budgets           Check the hermetic p50/p95 and cold-scan throughput
#                       against per-size budgets; exit non-zero on regression
#   --no-cache          Start the hermetic daemon with --no-query-cache so the
#                       latency figures are cold-path, not cache hits (F7)
#   --scaling           Also measure the cold scan with one rayon thread, to
#                       show the parallel speedup (F8)
#   --warm-start        Also measure a warm start from a snapshot (F6)
#   --stop-list-divisor N  Override the F12 stop-list divisor (0 disables; R10)
#   --no-token-table    Disable the F3 token table (R10)
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
do_memory=1
do_criterion=0
do_profile=0
json=0
keep=0
do_budgets=0
no_cache=0
do_scaling=0
do_warm_start=0
stop_list_divisor=""
no_token_table=0
files=5000
queries=30
duration=4
live_queries=3
records=1000000
socket_override=""

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --live) run_live=yes; run_hermetic=0 ;;
    --hermetic) run_live=no; run_hermetic=1 ;;
    --rescan) do_indexing=1 ;;
    --no-live-update) do_live_update=0 ;;
    --no-memory) do_memory=0 ;;
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
    --budgets) do_budgets=1 ;;
    --no-cache) no_cache=1 ;;
    --scaling) do_scaling=1 ;;
    --warm-start) do_warm_start=1 ;;
    --stop-list-divisor) stop_list_divisor="$2"; shift ;;
    --no-token-table) no_token_table=1 ;;
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

# ---- memory measurement ----------------------------------------------------
# The kernel tracks both the current resident set size (VmRSS) and its peak
# (VmHWM, the high-water mark) in /proc/<pid>/status. VmHWM captures the scan's
# transient peak even after the daemon trims its heap, so it is the peak-memory
# figure; VmRSS is the steady-state cost.
read_mem_kb() {
  awk -v f="$2:" '$1 == f {print $2}' "/proc/$1/status" 2>/dev/null
}

# Resolve the daemon PID bound to a socket by matching the socket path in each
# process's command line (the daemon is started with --socket <path>).
find_daemon_pid() {
  local sock="$1" pid
  for pid in /proc/[0-9]*; do
    pid="${pid#/proc/}"
    [[ -r "/proc/$pid/cmdline" ]] || continue
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF -- "$sock"; then
      printf '%s\n' "$pid"
      return 0
    fi
  done
  return 1
}

# Record the current and peak RSS of a daemon PID under a result key. When the
# entry count is known, also report the per-record memory cost: the end-to-end
# RSS per record, and — when the daemon exposes its index accounting (the live
# daemon, via D-Bus status) — the index's own bytes per record, i.e. how
# memory-efficient the in-memory cache is.
record_memory() {
  local key="$1" pid="$2" title="${3:-daemon}" entries="${4:-}" index_bytes="${5:-}"
  [[ "$do_memory" == 1 && -n "$pid" ]] || return 0
  local rss hwm
  rss="$(read_mem_kb "$pid" VmRSS)"
  hwm="$(read_mem_kb "$pid" VmHWM)"
  [[ -n "$rss" ]] || return 0
  hwm="${hwm:-$rss}"
  local msg="memory ($title): rss=$((rss / 1024)) MiB  peak=$((hwm / 1024)) MiB"
  local jobj="{\"rss_kb\": $rss, \"peak_rss_kb\": $hwm"
  if [[ -n "$entries" && "$entries" -gt 0 ]]; then
    local rss_bpr peak_bpr
    rss_bpr="$(awk -v r="$rss" -v n="$entries" 'BEGIN { printf "%.1f", r * 1024 / n }')"
    peak_bpr="$(awk -v r="$hwm" -v n="$entries" 'BEGIN { printf "%.1f", r * 1024 / n }')"
    msg+="  rss/record=${rss_bpr}B peak/record=${peak_bpr}B (n=$entries)"
    jobj+=", \"entries\": $entries, \"rss_bytes_per_record\": $rss_bpr, \"peak_rss_bytes_per_record\": $peak_bpr"
  fi
  if [[ -n "$index_bytes" && "$index_bytes" -gt 0 && -n "$entries" && "$entries" -gt 0 ]]; then
    local idx_bpr
    idx_bpr="$(awk -v b="$index_bytes" -v n="$entries" 'BEGIN { printf "%.1f", b / n }')"
    msg+="  index/record=${idx_bpr}B"
    jobj+=", \"index_bytes\": $index_bytes, \"index_bytes_per_record\": $idx_bpr"
  fi
  jobj+="}"
  say "$msg"
  add_result "$key" "$jobj"
}

# ---- performance budgets (F9) ----------------------------------------------
# Compare the hermetic p50/p95 and cold-scan throughput against per-size budgets.
# The budgets scale with the entry count and are deliberately generous so a slow
# shared CI runner does not flake, while still catching an order-of-magnitude
# regression. Exit non-zero on any violation.
check_budgets() {
  python3 - "$results" <<'PY'
import json
import sys

results_path = sys.argv[1]
data = {}
with open(results_path) as f:
    for line in f:
        line = line.rstrip("\n")
        if not line:
            continue
        key, _, val = line.partition("\t")
        try:
            data[key] = json.loads(val)
        except Exception:
            data[key] = val

idx = data.get("indexing")
entries = idx.get("entries") if isinstance(idx, dict) else None
if not entries:
    print("budgets: no hermetic entry count recorded; skipping")
    sys.exit(0)


def find(prefix):
    for k, v in data.items():
        if k.startswith(prefix) and isinstance(v, dict):
            return v
    return None


# p50 budget in ms = base + entries * per_record; the p95 budget is 2x the p50.
BASE = {"selective": 2.0, "broad": 2.0, "no-match": 2.0}
PER = {"selective": 0.00005, "broad": 0.0001, "no-match": 0.00005}
MIN_ENTRIES_PER_SEC = 50000

failures = []
for kind in ("selective", "broad", "no-match"):
    s = find(f"latency:{kind}")
    if not s:
        continue
    p50 = s["wall_ms"]["p50"]
    p95 = s["wall_ms"]["p95"]
    b50 = BASE[kind] + entries * PER[kind]
    b95 = b50 * 2
    ok = p50 <= b50 and p95 <= b95
    print(
        f"  budget {kind:9} p50={p50:8.3f}ms (<= {b50:7.2f})  "
        f"p95={p95:8.3f}ms (<= {b95:7.2f})  {'ok' if ok else 'FAIL'}"
    )
    if not ok:
        failures.append(kind)

if isinstance(idx, dict):
    eps = idx.get("entries_per_sec", 0)
    ok = eps >= MIN_ENTRIES_PER_SEC
    print(
        f"  budget cold-scan {eps:,.0f} entries/s (>= {MIN_ENTRIES_PER_SEC:,})  "
        f"{'ok' if ok else 'FAIL'}"
    )
    if not ok:
        failures.append("cold-scan")

if failures:
    print(f"budget check FAILED: {', '.join(failures)}", file=sys.stderr)
    sys.exit(1)
print("budget check passed")
PY
}

# ---- locate the binaries ---------------------------------------------------
daemon_bin=""
for cand in "$here/bin/omavrythingd" "$here/target/release/omavrythingd" /usr/bin/omavrythingd; do
  if [[ -x "$cand" ]]; then
    daemon_bin="$cand"
    break
  fi
done
cli_bin=""
for cand in "$here/bin/omavry" "$here/target/release/omavry" /usr/bin/omavry; do
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
import sys
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
    # Human-readable output goes to stderr so stdout stays clean for --json.
    if not s:
        print(f"  {title}: no samples", file=sys.stderr)
        return
    w, t = s["wall_ms"], s["took_ms"]
    print(
        f"  {title}: n={s['count']}  "
        f"wall p50={w['p50']}ms p95={w['p95']}ms p99={w['p99']}ms "
        f"max={w['max']}ms mean={w['mean']}ms  |  "
        f"daemon took_ms p50={t['p50']} p95={t['p95']} max={t['max']}",
        file=sys.stderr,
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
    if a.title:
        emit_latency(a.title, s)


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
    print(
        f"  windows observed: {result['windows']}  samples: {result['total']}",
        file=sys.stderr,
    )
    if result["windows"] == 0:
        print(
            "  note: no indexing window was observed. The periodic rescan is "
            "idle-gated (load, CPU idle, and the login1 idle hint), so an "
            "active desktop session vetoes it; it runs in an idle session or "
            "in CI. The live-update burst is not gated.",
            file=sys.stderr,
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
add_result "environment" "$(python3 -c 'import json,sys; print(json.dumps({"version": sys.argv[1], "cpus": sys.argv[2], "daemon": sys.argv[3]}))' "$version" "$cpus" "$daemon_bin")"

# ---- latency measurement helper -------------------------------------------
measure_latency() {
  local title="$1" query="$2" sock="$3" count="${4:-$queries}"
  # Scale the warmup to the sample count: a live query on a multi-million-entry
  # index costs hundreds of milliseconds, so a fixed 20-query warmup would
  # dominate the run.
  local warmup=$((count < 20 ? count : 20))
  # Always measure once and record the JSON result (the budget check reads it);
  # the human-readable line is written to stderr by the Python helper.
  local out
  out="$(python3 "$py" latency --socket "$sock" --query "$query" --count "$count" --warmup "$warmup" --json --title "$title")"
  # F7: with --no-cache the daemon has no memoization cache, so the figures are
  # cold-path; tag the key so the two runs are distinguishable in --json.
  local suffix=""
  if [[ "$no_cache" == 1 ]]; then
    suffix=":cold"
  fi
  add_result "latency:$title$suffix" "$out"
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
  live_index_bytes=""
  live_candidate=""
  live_candidate_postings=""
  live_candidate_lower=""
  live_candidate_tokens=""
  live_candidate_map=""
  live_candidate_off=""
  live_candidate_len=""
  live_candidate_stop=""
  if [[ -n "$cli_bin" ]]; then
    live_status="$("$cli_bin" status 2>/dev/null || true)"
    live_entries="$(sed -n 's/^entries: //p' <<<"$live_status")"
    live_index_bytes="$(sed -n 's/^memory_bytes: //p' <<<"$live_status")"
    live_candidate="$(sed -n 's/^candidate_bytes: //p' <<<"$live_status")"
    live_candidate_postings="$(sed -n 's/^candidate_postings: //p' <<<"$live_status")"
    live_candidate_lower="$(sed -n 's/^candidate_lower: //p' <<<"$live_status")"
    live_candidate_tokens="$(sed -n 's/^candidate_tokens: //p' <<<"$live_status")"
    live_candidate_map="$(sed -n 's/^candidate_map: //p' <<<"$live_status")"
    live_candidate_off="$(sed -n 's/^candidate_off: //p' <<<"$live_status")"
    live_candidate_len="$(sed -n 's/^candidate_len: //p' <<<"$live_status")"
    live_candidate_stop="$(sed -n 's/^candidate_stop: //p' <<<"$live_status")"
  fi
  if [[ -n "$live_entries" ]]; then
    say "index entries: $live_entries"
    add_result "live" "$(python3 -c 'import json,sys; print(json.dumps({"socket": sys.argv[1], "entries": int(sys.argv[2])}))' "$live_socket" "$live_entries")"
  fi
  # The live index is large (millions of entries) and a broad query scans all of
  # it, so the live phase uses a smaller sample count than the hermetic one.
  for q in "a" "config" "zzz-no-such-file-zzzz"; do
    measure_latency "live \"$q\"" "$q" "$live_socket" "$live_queries"
  done
  if [[ "$do_memory" == 1 ]]; then
    live_pid="$(find_daemon_pid "$live_socket" || true)"
    if [[ -n "$live_pid" ]]; then
      say "daemon pid: $live_pid"
      record_memory "memory:live" "$live_pid" "live" "$live_entries" "$live_index_bytes"
    else
      say "memory: could not resolve the live daemon pid for $live_socket"
    fi
  fi
  # Per-component candidate-index breakdown (F11), from the daemon's own
  # accounting. This is the memory the n-gram index costs, split by component.
  if [[ -n "$live_candidate" && "$live_candidate" -gt 0 && -n "$live_entries" && "$live_entries" -gt 0 ]]; then
    cand_bpr="$(awk -v b="$live_candidate" -v n="$live_entries" 'BEGIN { printf "%.1f", b / n }')"
    say "candidate index: ${live_candidate} B (${cand_bpr} B/record)  postings=${live_candidate_postings} lower=${live_candidate_lower} tokens=${live_candidate_tokens} map=${live_candidate_map} off=${live_candidate_off} len=${live_candidate_len} stop=${live_candidate_stop}"
    add_result "candidate:live" "$(python3 -c 'import json,sys; print(json.dumps({"bytes": int(sys.argv[1]), "bytes_per_record": float(sys.argv[2]), "postings": int(sys.argv[3]), "lower": int(sys.argv[4]), "tokens": int(sys.argv[5]), "map": int(sys.argv[6]), "off": int(sys.argv[7]), "len": int(sys.argv[8]), "stop": int(sys.argv[9])}))' "$live_candidate" "$cand_bpr" "$live_candidate_postings" "$live_candidate_lower" "$live_candidate_tokens" "$live_candidate_map" "$live_candidate_off" "$live_candidate_len" "$live_candidate_stop")"
  fi
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
    # A `--snapshot <path>` in `extra` means the caller wants persistence, so the
    # default `--no-snapshot` is dropped (F6 warm-start measurement).
    local snap_flag=(--no-snapshot)
    local cache_flag=()
    for a in "${extra[@]}"; do
      if [[ "$a" == "--snapshot" ]]; then
        snap_flag=()
      fi
    done
    # F7: --no-cache starts the daemon without the query-result memoization cache
    # so the latency figures are cold-path.
    if [[ "$no_cache" == 1 ]]; then
      cache_flag=(--no-query-cache)
    fi
    # R10 measurement knobs: the stop-list divisor and the token table.
    local env_vars=()
    if [[ -n "$stop_list_divisor" ]]; then
      env_vars+=("OMAVRYTHING_STOP_LIST_DIVISOR=$stop_list_divisor")
    fi
    if [[ "$no_token_table" == 1 ]]; then
      env_vars+=("OMAVRYTHING_TOKEN_TABLE=0")
    fi
    local env_prefix=()
    if [[ ${#env_vars[@]} -gt 0 ]]; then
      env_prefix=(env "${env_vars[@]}")
    fi
    : > "$log"
    # A previous run leaves its socket file behind; remove it so the wait below
    # cannot be satisfied by a stale, unlistened socket.
    rm -f "$sock"
    "${env_prefix[@]}" "$daemon_bin" --no-config --root "$root" --socket "$sock" \
      --no-dbus --no-ntfs --log-level info "${snap_flag[@]}" "${cache_flag[@]}" "${extra[@]}" \
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
      add_result "indexing" "{\"entries\": $entries, \"elapsed_ms\": $elapsed, \"entries_per_sec\": $eps}"
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
      out="$(python3 "$py" report --samples "$tmp/samples.txt" --windows "$tmp/windows.txt" --json)"
      add_result "during_rescan" "$out"
    fi
    record_memory "memory:hermetic" "$daemon_pid" "hermetic" "$entries"
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
      # The index holds the cold-scan entries plus the burst files once the
      # watcher has applied them, so the per-record figure uses that total.
      burst_entries="$(sed -n 's/.*entries=\([0-9]*\).*/\1/p' <<<"$(grep -m1 'indexed entries=' "$log" || true)")"
      [[ -n "$burst_entries" ]] && burst_entries=$((burst_entries + burst_files))
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
      out="$(python3 "$py" report --samples "$tmp/burst-samples.txt" --windows "$tmp/burst-windows.txt" --json)"
      add_result "during_live_update" "$out"
      record_memory "memory:hermetic_live_update" "$daemon_pid" "hermetic live-update" "$burst_entries"
      stop_daemon
    else
      say "error: the hermetic daemon did not start for the live-update run" >&2
    fi
  fi

  # --- run C: single-thread cold scan (F8 scaling) -------------------------
  if [[ "$do_scaling" == 1 ]]; then
    say "cold scan with one rayon thread (F8 scaling):"
    if RAYON_NUM_THREADS=1 start_daemon --scan-interval 0; then
      scan_line="$(grep -m1 'indexed entries=' "$log" || true)"
      entries1="$(sed -n 's/.*entries=\([0-9]*\).*/\1/p' <<<"$scan_line")"
      elapsed1="$(sed -n 's/.*elapsed_ms=\([0-9]*\).*/\1/p' <<<"$scan_line")"
      if [[ -n "$entries1" && -n "$elapsed1" && "$elapsed1" -gt 0 ]]; then
        eps1="$(python3 -c "print(round($entries1 / ($elapsed1 / 1000.0), 1))")"
        say "cold scan (1 thread): $entries1 entries in ${elapsed1} ms ($eps1 entries/s)"
        add_result "indexing_1thread" "{\"entries\": $entries1, \"elapsed_ms\": $elapsed1, \"entries_per_sec\": $eps1}"
      else
        say "cold scan (1 thread): could not parse the scan line"
      fi
      stop_daemon
    else
      say "error: the single-thread hermetic daemon did not start" >&2
    fi
  fi

  # --- run D: warm start from a snapshot (F6) ------------------------------
  if [[ "$do_warm_start" == 1 ]]; then
    snap="$tmp/index.snapshot"
    say "warm start from a snapshot (F6):"
    # The first run writes the snapshot on clean shutdown; the second loads it.
    if start_daemon --scan-interval 0 --snapshot "$snap" --snapshot-interval 0; then
      stop_daemon
      if [[ -f "$snap" ]]; then
        if start_daemon --scan-interval 0 --snapshot "$snap" --snapshot-interval 0; then
          warm_line="$(grep -m1 'warm-started from snapshot' "$log" || true)"
          warm_ms="$(sed -n 's/.*elapsed_ms=\([0-9]*\).*/\1/p' <<<"$warm_line")"
          if [[ -n "$warm_ms" ]]; then
            say "warm start: ${warm_ms} ms"
            add_result "warm_start" "{\"elapsed_ms\": $warm_ms}"
          else
            say "warm start: could not parse the warm-start line"
          fi
          stop_daemon
        fi
      else
        say "warm start: no snapshot was written"
      fi
    else
      say "error: the hermetic daemon did not start for the warm-start run" >&2
    fi
  fi
fi

# ---- performance budgets (F9) ----------------------------------------------
if [[ "$do_budgets" == 1 ]]; then
  section "performance budgets"
  check_budgets
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
