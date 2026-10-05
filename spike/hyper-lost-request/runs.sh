#!/usr/bin/env bash
# Runs the full controllers_it suite N times with hyper's internal tracing
# and keeps the logs of a run that lost a request (balerix#129).
# usage: runs.sh <label> <count>
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
source "$here/env.sh"
label=$1 count=$2
out="$W/target/spike-logs/$label"
mkdir -p "$out"
export RUST_LOG="${RUST_LOG:-info,spike=trace,balerix_operator=debug,kube_client=debug,tower_http=debug,hyper=trace,hyper_util=trace}"
cd "$W/operator"
for i in $(seq 1 "$count"); do
  export SPIKE_LOG_DIR="$out/run-$i"
  start=$(date +%s)
  cargo nextest run --config-file .config/nextest.toml --test controllers_it \
    --no-fail-fast >"$out/run-$i.nextest" 2>&1
  rc=$?
  lost=$(grep -l "SPIKE lost" "$SPIKE_LOG_DIR"/*.log 2>/dev/null | wc -l)
  summary=$(grep -E "Summary" "$out/run-$i.nextest" | tail -1)
  echo "run $i rc=$rc lost_files=$lost secs=$(( $(date +%s) - start )) load=$(cut -d' ' -f1-3 /proc/loadavg) $summary" | tee -a "$out/summary.txt"
  if [[ $lost -eq 0 ]]; then rm -rf "$SPIKE_LOG_DIR"; fi
  rm -rf "$CARGO_TARGET_DIR"/tmp/envtest-* "$CARGO_TARGET_DIR"/tmp/*-[0-9]*
done
