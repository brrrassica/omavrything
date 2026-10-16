#!/usr/bin/env bash
# Profile daemon RSS before (owned) and after (evicted) eviction, N passes.
set -euo pipefail

pid="$(systemctl show -p MainPID --value omavrythingd)"
echo "daemon pid: $pid"

rss_kb() { awk '/VmRSS/{print $2}' "/proc/$pid/status"; }
peak_kb() { awk '/VmHWM/{print $2}' "/proc/$pid/status"; }

echo "pass,owned_rss_mib,evicted_rss_mib,peak_rss_mib"

for i in 1 2 3 4 5; do
  # Load: a mutation (rescan) materializes the owned index; wait until the
  # daemon reports a resident index.
  omavry rescan >/dev/null 2>&1
  owned=""
  for _ in $(seq 1 60); do
    sleep 1
    len="$(omavry status --json | sed -n 's/.*"records_len":"\([0-9]*\)".*/\1/p')"
    if [[ -n "$len" && "$len" != "0" ]]; then
      owned="$(rss_kb)"
      break
    fi
  done
  [[ -n "$owned" ]] || { echo "pass $i: index never materialized" >&2; exit 1; }

  # Evict: stay idle past evict_idle_secs (10 s), then confirm eviction.
  evicted=""
  for _ in $(seq 1 30); do
    sleep 2
    len="$(omavry status --json | sed -n 's/.*"records_len":"\([0-9]*\)".*/\1/p')"
    if [[ "$len" == "0" ]]; then
      evicted="$(rss_kb)"
      break
    fi
  done
  [[ -n "$evicted" ]] || { echo "pass $i: never evicted" >&2; exit 1; }

  peak="$(peak_kb)"
  printf '%d,%.1f,%.1f,%.1f\n' "$i" \
    "$(awk -v k="$owned" 'BEGIN{print k/1024}')" \
    "$(awk -v k="$evicted" 'BEGIN{print k/1024}')" \
    "$(awk -v k="$peak" 'BEGIN{print k/1024}')"
done
