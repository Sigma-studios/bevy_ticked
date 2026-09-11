#!/usr/bin/env bash
# Run the netpeer example as real processes over real WebRTC and grade the session.
#
#   scripts/netpeers.sh session        # a host and a client agree tick for tick
#   scripts/netpeers.sh join           # a client joining sees the host's world within the deadline
#   scripts/netpeers.sh soak [N]       # `session` N times (default 10), stop at the first failure
#
# Env: SIGNALLING_SERVER_URL (default: an in-process server started by the test harness is not
#      available here, so ws://localhost:9090/ws — run `cargo run -p bevy_ensemble_webrtc
#      --features server --bin bevy_ensemble_webrtc_server` first), OUT (log dir,
#      default target/netpeers), CLIENTS (default 1), TICKS (default 400).
#
# The rules, from una_zombies' `scripts/two-peer.sh`:
# - Assert on the peer under test. The client's checksum log is compared with the host's; a
#   host that quietly did the work itself cannot pass.
# - Cut the logs at LOG_SESSION_START. The bar is zero WARN/ERROR from our own crates after
#   the session is up; the transport's ICE chatter is filtered by source, not by level.
# - A transport that never connected is INCONCLUSIVE (exit 3), never a failed assertion:
#   it names the thing to go and look at.

set -uo pipefail

CHECK="${1:-}"
COUNT="${2:-10}"
export SIGNALLING_SERVER_URL="${SIGNALLING_SERVER_URL:-ws://localhost:9090/ws}"
OUT="${OUT:-target/netpeers}"
CLIENTS="${CLIENTS:-1}"
TICKS="${TICKS:-400}"
BIN=target/debug/examples/netpeer

usage() {
    echo "usage: scripts/netpeers.sh <session|join|soak [N]>"
    exit 2
}
[[ -z "$CHECK" ]] && usage

if [[ ! -x "$BIN" ]]; then
    echo "building the netpeer example"
    cargo build -p bevy_ticked_networking_ensemble --example netpeer || exit 2
fi

if pgrep -f "examples/netpeer --" >/dev/null 2>&1; then
    echo "another netpeer is already running, and a client joins whichever lobby the server"
    echo "lists first — stop it, or point SIGNALLING_SERVER_URL at a different server."
    exit 2
fi

mkdir -p "$OUT"

run_session() {
    local tag="$1"
    local dir="$OUT/$tag"
    rm -rf "$dir"; mkdir -p "$dir"
    RUST_LOG=info "$BIN" --role host --walk --ticks "$TICKS" --linger 6000 \
        --out "$dir/host.checksums" > "$dir/host.log" 2>&1 &
    local pids=($!)
    for ((i = 0; i < CLIENTS; i++)); do
        RUST_LOG=info "$BIN" --role client --index "$i" --walk --fire --ticks "$TICKS" \
            --linger 2000 --out "$dir/client-$i.checksums" > "$dir/client-$i.log" 2>&1 &
        pids+=($!)
    done
    local status=0
    for pid in "${pids[@]}"; do
        wait "$pid"; local code=$?
        if [[ $code -eq 3 ]]; then status=3; elif [[ $code -ne 0 && $status -ne 3 ]]; then status=1; fi
    done
    echo "$dir"
    return $status
}

noise_after_start() {
    local log="$1"
    sed 's/\x1b\[[0-9;]*m//g' "$log" \
        | awk '/LOG_SESSION_START/{f=1; next} f' \
        | grep -E " (WARN|ERROR) " \
        | grep -v -E "webrtc|bevy_ensemble_sockets|left the lobby" || true
}

compare_logs() {
    local host="$1" client="$2"
    # Join on tick; report the first differing hash, the overlap, and whether anything moved.
    awk 'NR==FNR { h[$1]=$2; p[$1]=$4; next }
         ($1 in h) { n++; if (p[$1] != last) { moved++; last = p[$1] }
                     if (h[$1] != $2 && !d) { d = $1; hh = h[$1]; ch = $2 } }
         END { printf "overlap=%d moved=%d", n, moved; if (d) printf " diverged=%s host=%s client=%s", d, hh, ch; print "" }' \
        "$host" "$client"
}

grade_session() {
    local dir="$1" ok=0
    for ((i = 0; i < CLIENTS; i++)); do
        local result
        result=$(compare_logs "$dir/host.checksums" "$dir/client-$i.checksums")
        echo "client-$i vs host: $result"
        case "$result" in
            *diverged=*) echo "FAIL: client-$i diverged from the host"; ok=1 ;;
        esac
        local overlap=${result#overlap=}; overlap=${overlap%% *}
        if [[ "${overlap:-0}" -lt 120 ]]; then echo "FAIL: only $overlap shared ticks"; ok=1; fi
        local moved; moved=$(echo "$result" | sed -n 's/.*moved=\([0-9]*\).*/\1/p')
        if [[ "${moved:-0}" -lt 2 ]]; then echo "FAIL: the world never moved"; ok=1; fi
    done
    for log in "$dir"/*.log; do
        local noise; noise=$(noise_after_start "$log")
        if [[ -n "$noise" ]]; then
            echo "FAIL: $(basename "$log") logged after the session started:"; echo "$noise"; ok=1
        fi
        grep -q LOG_SESSION_START "$log" || { echo "FAIL: $(basename "$log") never started a session"; ok=1; }
    done
    return $ok
}

case "$CHECK" in
    session)
        dir=$(run_session session); code=$?
        [[ $code -eq 3 ]] && { echo "=== session: INCONCLUSIVE — the transport never connected ==="; exit 3; }
        [[ $code -ne 0 ]] && { echo "=== session: FAIL — a peer exited non-zero (see $dir) ==="; exit 1; }
        grade_session "$dir" && echo "=== session: PASS ===" || exit 1
        ;;
    join)
        dir=$(run_session join); code=$?
        [[ $code -eq 3 ]] && { echo "=== join: INCONCLUSIVE ==="; exit 3; }
        [[ $code -ne 0 ]] && { echo "=== join: FAIL (see $dir) ==="; exit 1; }
        # The client's own log: its session start relative to its first line, in seconds.
        for ((i = 0; i < CLIENTS; i++)); do
            log="$dir/client-$i.log"
            strip='s/\x1b\[[0-9;]*m//g'
            first=$(head -1 "$log" | sed "$strip" | awk '{print $1}')
            start=$(grep -m1 LOG_SESSION_START "$log" | sed "$strip" | awk '{print $1}')
            [[ -z "$start" ]] && { echo "FAIL: client-$i never saw the host's world"; exit 1; }
            echo "client-$i: started at $first, saw the host's world at $start"
        done
        echo "=== join: PASS ==="
        ;;
    soak)
        for ((n = 1; n <= COUNT; n++)); do
            dir=$(run_session "soak-$n"); code=$?
            [[ $code -eq 3 ]] && { echo "=== soak: INCONCLUSIVE at run $n ==="; exit 3; }
            [[ $code -ne 0 ]] && { echo "=== soak: FAIL at run $n (see $dir) ==="; exit 1; }
            grade_session "$dir" || { echo "=== soak: FAIL at run $n ==="; exit 1; }
            echo "run $n/$COUNT ok"
        done
        echo "=== soak: PASS ($COUNT runs) ==="
        ;;
    *) usage ;;
esac
