#!/usr/bin/env bash
# Runs the conformance suite and the live JSON-LD check against the demo,
# once for each kind of CORS setting: same origin only, any origin, and
# listed origins. Needs conformance/requirements.txt installed; PYTHON picks
# the interpreter (default python3).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --locked
work=$(mktemp -d)
pids=()
cleanup() {
  kill "${pids[@]}" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

python=${PYTHON:-python3}
port=18080
for cors in "" "*" "https://portal.example.org,http://localhost:3000"; do
  port=$((port + 1))
  base="http://127.0.0.1:$port"
  # A missing DEV_ENV_FILE keeps a local dev.env out of the run.
  DEV_ENV_FILE="$work/none.env" GLOBUS_MOCK=true BASE_URL="$base" BIND_ADDR="127.0.0.1:$port" \
    CORS_ALLOWED_ORIGINS="$cors" IDP_SIGNING_KEY="$work/key-$port.pem" \
    target/debug/idp_redirect_demo >"$work/server-$port.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 120); do
    curl -s -o /dev/null "$base/api/v1/institutions" && break
    sleep 0.5
  done
  if ! curl -s -o /dev/null "$base/api/v1/institutions"; then
    cat "$work/server-$port.log"
    exit 1
  fi

  echo "== CORS setting: '${cors}'"
  API_BASE_URL="$base" API_CORS="$cors" PYTHONDONTWRITEBYTECODE=1 \
    "$python" -m pytest -q -p no:cacheprovider conformance
  "$python" scripts/check_jsonld.py --url "$base"
done
