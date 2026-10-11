#!/usr/bin/env bash
# tune.sh — xlite worker-thread tuning evaluation on the k6/mock rig.
#
# Three phases (see docs/manual/loadtest-tune.md for the method):
#   1. worker ladder: one round per candidate (default 4 8 16 32 64),
#      fixed stream shape and VU, only TOKIO_WORKER_THREADS varies
#   2. winner pick: best fixed-window completion rate (tie -> cheaper CPU)
#   3. validation: interleaved winner vs baseline, N rounds each
#
# The stream shape is the first decision: chunk rate (chunks/s per stream)
# decides the answer. From the D/F campaign: <=4 chunks/s -> 8 threads;
# 50 chunks/s -> 32 threads (+135%); between -> run this script.
#
# usage:
#   tune.sh --chunks 3000 --interval-ms 20 --vus 40000            # F shape
#   tune.sh --chunks 500 --interval-ms 250 --vus 16000 --workers "8 16 32"
#   tune.sh --chunks 50 --interval-ms 10 --vus 200 --steady 20s --repeats 1 --dry-run
#
# Options (env override in parentheses):
#   --chunks N          content chunks per stream (MOCK out_tokens)   [required]
#   --interval-ms N     chunk interval ms (MOCK token_interval)       [required]
#   --vus N             k6 VU level                                   [required]
#   --workers "L"       ladder candidates         (TUNE_WORKERS, default "4 8 16 32 64")
#   --baseline N        validation compare arm     (TUNE_BASELINE, default 8)
#   --steady DUR        steady duration           (TUNE_STEADY, default 640s)
#   --warmup DUR        warmup ramp               (TUNE_WARMUP, default 30s)
#   --repeats N         interleaved rounds per arm (TUNE_REPEATS, default 3)
#   --ttft N            mock first-token delay ms  (TUNE_TTFT, default 500)
#   --skip-validation   ladder only
#   --dry-run           print the plan and exit (self-check; touches nothing)
#
# Rig assumptions (override via env): 24 mocks on 127.0.0.1:8081-8104
# (TUNE_MOCK_BASE/TUNE_MOCK_COUNT), gateway binary TUNE_BIN (default
# /home/bodesi/p1/xlite-after), gateway base image TUNE_IMG (default xlite:5dbg),
# k6 image TUNE_K6 (default k6-customer:latest), k6 dir TUNE_K6DIR (default
# /home/bodesi/v88t), sqlite catalog TUNE_DB (default /home/bodesi/p3/db.sqlite).
# Farm config is RESTORED on exit (also on failure).
set -uo pipefail

CHUNKS=""; INTERVAL=""; VUS=""
WORKERS="${TUNE_WORKERS:-4 8 16 32 64}"
BASELINE="${TUNE_BASELINE:-8}"
STEADY="${TUNE_STEADY:-640s}"
WARMUP="${TUNE_WARMUP:-30s}"
REPEATS="${TUNE_REPEATS:-3}"
TTFT="${TUNE_TTFT:-500}"
BIN="${TUNE_BIN:-/home/bodesi/p1/xlite-after}"
IMG="${TUNE_IMG:-xlite:5dbg}"
K6="${TUNE_K6:-k6-customer:latest}"
K6DIR="${TUNE_K6DIR:-/home/bodesi/v88t}"
DB="${TUNE_DB:-/home/bodesi/p3/db.sqlite}"
MOCK_BASE="${TUNE_MOCK_BASE:-8081}"
MOCK_COUNT="${TUNE_MOCK_COUNT:-24}"
OUT="${TUNE_OUT:-/home/bodesi/tune-$(date +%Y%m%d-%H%M%S)}"
DRY=0; SKIPVAL=0

while [ $# -gt 0 ]; do
  case "$1" in
    --chunks) CHUNKS="$2"; shift 2;;
    --interval-ms) INTERVAL="$2"; shift 2;;
    --vus) VUS="$2"; shift 2;;
    --workers) WORKERS="$2"; shift 2;;
    --baseline) BASELINE="$2"; shift 2;;
    --steady) STEADY="$2"; shift 2;;
    --warmup) WARMUP="$2"; shift 2;;
    --repeats) REPEATS="$2"; shift 2;;
    --ttft) TTFT="$2"; shift 2;;
    --out) OUT="$2"; shift 2;;
    --dry-run) DRY=1; shift;;
    --skip-validation) SKIPVAL=1; shift;;
    -h|--help) sed -n '2,45p' "$0"; exit 0;;
    *) echo "unknown option: $1" >&2; exit 2;;
  esac
done

# ---- arg validation (fail loud) ----
for v in CHUNKS INTERVAL VUS; do
  eval "val=\$$v"
  if ! printf '%s' "$val" | grep -qE '^[0-9]+$' || [ "$val" -le 0 ]; then
    echo "ERROR: --${v,,} must be a positive integer (got '${val}')" >&2; exit 2
  fi
done
for w in $WORKERS; do
  printf '%s' "$w" | grep -qE '^[0-9]+$' || { echo "ERROR: bad worker value '$w'" >&2; exit 2; }
done
printf '%s' "$REPEATS" | grep -qE '^[1-9][0-9]*$' || { echo "ERROR: --repeats must be >=1" >&2; exit 2; }

STREAM_MS=$(( CHUNKS * INTERVAL ))
STREAM_S=$(awk "BEGIN{printf \"%.1f\", $STREAM_MS/1000 + $TTFT/1000}")
MAXVUS=$(( VUS * 5 / 4 ))
DEMAND=$(awk "BEGIN{printf \"%.1f\", $VUS / $STREAM_S}")
CHUNK_RATE=$(awk "BEGIN{printf \"%.1f\", 1000/$INTERVAL}")

echo "=== tune.sh plan ==="
echo "stream shape : ${CHUNKS} chunks x ${INTERVAL}ms = ${CHUNK_RATE} chunks/s/stream, ~${STREAM_S}s streams (ttft=${TTFT}ms)"
echo "load         : VUS=${VUS} (MAX_VUS=${MAXVUS}) warmup=${WARMUP} steady=${STEADY}"
echo "ladder       : workers = ${WORKERS}"
if [ "$SKIPVAL" = 0 ]; then
  echo "validation   : winner vs baseline=${BASELINE}, ${REPEATS} rounds each, interleaved"
else
  echo "validation   : skipped (--skip-validation)"
fi
echo "demand ceiling (approx): ${DEMAND} iter/s = VUS / stream seconds"
echo "output dir   : ${OUT}"
echo "k6 feed      : KEEP_BODY=true KEEP_SAMPLE=0.1 CHUNK_MAX=64 STREAM_TIMEOUT=600000 GRACEFUL_STOP=900s OUT_TOKENS=${CHUNKS}"
echo "runtime est  : ladder $(( $(echo $WORKERS | wc -w) * $(echo "${STEADY%[smh]*}") / 60 + 2 ))min + validation $(( REPEATS * 2 * $(echo "${STEADY%[smh]*}") / 60 + 3 ))min (rough)"
if [ "$DRY" = 1 ]; then echo "(dry-run: nothing touched)"; exit 0; fi

# ---- probe self-checks ----
command -v docker >/dev/null || { echo "ERROR: docker not found" >&2; exit 1; }
curl -sf -m 5 -X POST "http://127.0.0.1:${MOCK_BASE}/_config" >/dev/null || { echo "ERROR: mock farm not reachable at ${MOCK_BASE}" >&2; exit 1; }
[ -f "$BIN" ] || { echo "ERROR: gateway binary not found: $BIN" >&2; exit 1; }
[ -f "$K6DIR/k6.js" ] || { echo "ERROR: k6.js not found under $K6DIR" >&2; exit 1; }
[ -f "$DB" ] || { echo "ERROR: catalog db not found: $DB" >&2; exit 1; }

mkdir -p "$OUT"
sha256sum "$K6DIR/k6.js" > "$OUT/fingerprints.txt"
md5sum "$BIN" >> "$OUT/fingerprints.txt"
cp -f "$DB"* "$OUT/" 2>/dev/null

# farm shape + restore trap
MOCK_END=$(( MOCK_BASE + MOCK_COUNT - 1 ))
curl -s -X POST "http://127.0.0.1:${MOCK_BASE}/_config" > "$OUT/farm_config_orig.txt"
restore_farm() {
  for p in $(seq "$MOCK_BASE" "$MOCK_END"); do
    curl -s -X POST "http://127.0.0.1:$p/_config?ttft=${TTFT}&out_tokens=500&token_interval=10&batch=1" >/dev/null
  done
}
trap restore_farm EXIT
for p in $(seq "$MOCK_BASE" "$MOCK_END"); do
  curl -s -X POST "http://127.0.0.1:$p/_config?ttft=${TTFT}&out_tokens=${CHUNKS}&token_interval=${INTERVAL}&batch=1" >/dev/null
done
curl -s -X POST "http://127.0.0.1:${MOCK_BASE}/_config" > "$OUT/farm_config_batch.txt"

win_bounds() { # k6.log start end -> one line
  python3 - "$1" "$2" "$3" <<'PYEOF'
import re, sys
rows = []
for line in open(sys.argv[1], errors="ignore"):
    m = re.search(r"running \((\d+)m([\d.]+)s\), \d+/\d+ VUs, (\d+) complete", line)
    if m:
        rows.append((int(m.group(1)) * 60 + float(m.group(2)), int(m.group(3))))
if not rows:
    print("bounds=NO_PROGRESS_LINES"); sys.exit(0)
a = min(rows, key=lambda r: abs(r[0] - float(sys.argv[2])))
b = min(rows, key=lambda r: abs(r[0] - float(sys.argv[3])))
dt = b[0] - a[0]
print(f"bounds=({a[0]:.1f}s,{a[1]})->({b[0]:.1f}s,{b[1]}) rate={(b[1]-a[1])/max(dt,0.001):.1f}/s")
PYEOF
}

cores_used() { # res.tsv -> gw cores
  python3 - "$1" <<'PYEOF'
import sys
rows = [l.split() for l in open(sys.argv[1]).read().splitlines()[1:]]
if len(rows) < 2:
    print("?"); raise SystemExit
t0, g0 = float(rows[0][0]), int(rows[0][1])
t1, g1 = float(rows[-1][0]), int(rows[-1][1])
print(f"{(g1-g0)/100/max(t1-t0,0.001):.1f}")
PYEOF
}

STEADY_S=$(echo "$STEADY" | sed 's/[^0-9.]//g')
WIN_START=$(awk "BEGIN{printf \"%.0f\", $STEADY_S * 0.1}")
WIN_END=$(awk "BEGIN{printf \"%.0f\", $STEADY_S * 1.0}")

sampler() { # dir gwpid
  python3 - "$1" "$2" <<'PYEOF' &
import sys, time
d, gwpid = sys.argv[1], int(sys.argv[2])
def cpu(pid):
    try:
        f = open(f"/proc/{pid}/stat").read(); rp = f.rfind(")")
        p = f[rp+2:].split(); return int(p[11])+int(p[12])
    except Exception: return 0
f = open(f"{d}/res.tsv","w")
f.write("ts\tgw\n")
t0 = time.time()
while time.time()-t0 < 3600:
    f.write(f"{round(time.time()-t0,1)}\t{cpu(gwpid)}\n"); f.flush(); time.sleep(2)
PYEOF
  echo $! > "$1/sampler.pid"
}

round_run() { # tag workers -> appends a row to report.md
  local tag=$1 w=$2
  local d="$OUT/$tag"; mkdir -p "$d"
  cp -f "$DB"* "$d/" 2>/dev/null
  docker rm -f tune-gw tune-k6 >/dev/null 2>&1
  docker run -d --name tune-gw --network host --ulimit nofile=1048576:1048576 \
    -v "$d:/data" -v "$BIN:/usr/local/bin/xlite:ro" \
    -e DATABASE_URL=sqlite:/data/db.sqlite -e ADMIN_PASSWORD=x \
    -e TOKIO_WORKER_THREADS=$w -e XLITE_UPSTREAM_H2C=1 \
    -e XLITE_UPSTREAM_STREAM_CAP=250 -e XLITE_UPSTREAM_MAX_CONNS_PER_HOST=32 \
    -e PORT=18082 -e LISTEN_HOST=0.0.0.0 "$IMG" > "$d/dockerrun.out" 2> "$d/dockerrun.err"
  sleep 5
  local pre=000
  for try in 1 2 3; do
    pre=$(curl -s -o /dev/null -w '%{http_code}' -m 30 -X POST http://127.0.0.1:18082/v1/chat/completions \
      -H "Authorization: Bearer sk-loadtest" -H 'Content-Type: application/json' \
      -d '{"model":"mock-model","stream":true,"messages":[{"role":"user","content":"hi"}]}')
    [ "$pre" = "200" ] && break
    sleep 20
  done
  local gwpid=$(docker inspect tune-gw --format '{{.State.Pid}}' 2>/dev/null)
  gwpid=${gwpid:-0}
  sampler "$d" "$gwpid"
  sleep 2
  docker run -d --name tune-k6 --network host --ulimit nofile=1048576:1048576 -m 45g \
    -v "$K6DIR:/k6" "$K6" run \
    -e TARGET_URL=http://127.0.0.1:18082/v1/chat/completions -e TOKEN=sk-loadtest \
    -e MODEL=mock-model -e STREAM=true -e IN_TOKENS=100 -e OUT_TOKENS=$CHUNKS \
    -e KEEP_BODY=true -e KEEP_SAMPLE=0.1 -e CHUNK_MAX=64 \
    -e STREAM_TIMEOUT=600000 -e GRACEFUL_STOP=900s \
    -e VUS=$VUS -e MAX_VUS=$MAXVUS -e DURATION=$STEADY -e WARMUP=$WARMUP \
    /k6/k6.js > "$d/k6.log" 2>&1
  docker wait tune-k6 >> "$d/k6.log" 2>/dev/null
  docker logs tune-k6 >> "$d/k6.log" 2>/dev/null
  kill $(cat "$d/sampler.pid") 2>/dev/null
  docker rm -f tune-gw tune-k6 >/dev/null 2>&1
  local wb=$(win_bounds "$d/k6.log" "$WIN_START" "$WIN_END")
  local rate=$(echo "$wb" | sed 's/.*rate=\([0-9.]*\)\/s.*/\1/')
  local ttft=$(grep -m1 'ttft_ms' "$d/k6.log" | sed 's/.*avg=\([0-9.]*\).*/\1/')
  local dur=$(grep -m1 'http_req_duration' "$d/k6.log" | sed 's/.*avg=\([^ ]*\).*/\1/')
  local fail=$(grep -m1 'http_req_failed' "$d/k6.log" | sed 's/.*: *\([0-9.]*\)%.*/\1/')
  local cores=$(cores_used "$d/res.tsv")
  echo "| $tag | $w | $rate | $ttft | $dur | $fail | $cores | $wb |" >> "$OUT/report.md"
  echo "  -> $tag workers=$w rate=$rate/s ttft=${ttft}ms dur=$dur fail=${fail}% gw=${cores} cores"
  sleep 60   # cooldown / resource recovery
}

echo "" >> "$OUT/report.md"
echo "| 轮次 | workers | 窗口速率(/s) | TTFT avg | dur avg | 失败% | 网关核 | 窗口边界 |" >> "$OUT/report.md"
echo "|---|---|---|---|---|---|---|---|" >> "$OUT/report.md"

# ---- phase 1: ladder ----
echo "--- phase 1: worker ladder ---"
for w in $WORKERS; do
  round_run "ladder_w$w" "$w"
done

# ---- phase 2: winner ----
# best window rate; TIES go to the smaller worker count (cheaper) -> -k1,1nr -k2,2n
WINNER=$(grep '^| ladder_' "$OUT/report.md" | awk -F'|' '{gsub(/ /,"",$4); gsub(/\/s/,"",$4); print $4, $3}' | sort -k1,1nr -k2,2n | head -1 | awk '{print $2}')
[ -n "$WINNER" ] || { echo "ERROR: winner pick failed" >&2; exit 1; }
echo "--- phase 2: winner = workers=$WINNER (best window rate; ties -> fewer workers) ---"

# ---- phase 3: validation (interleaved) ----
if [ "$SKIPVAL" = 0 ]; then
  echo "--- phase 3: validation $WINNER vs $BASELINE x$REPEATS interleaved ---"
  echo "" >> "$OUT/report.md"
  echo "## validation ($WINNER vs $BASELINE)" >> "$OUT/report.md"
  echo "| 轮次 | workers | 窗口速率(/s) | TTFT avg | dur avg | 失败% | 网关核 | 窗口边界 |" >> "$OUT/report.md"
  echo "|---|---|---|---|---|---|---|---|" >> "$OUT/report.md"
  for i in $(seq 1 "$REPEATS"); do
    round_run "val_${i}_w$BASELINE" "$BASELINE"
    round_run "val_${i}_w$WINNER" "$WINNER"
  done
fi

echo "=== tune.sh done: $OUT/report.md ==="
