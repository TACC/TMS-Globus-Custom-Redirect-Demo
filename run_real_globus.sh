#!/usr/bin/env bash
# Run the demo against real Globus Auth (Ubuntu / WSL2). All settings come from
# dev.env (untracked; template: dev.env.example), which the app reads itself.
set -euo pipefail
cd "$(dirname "$0")"

if [ ! -f "${DEV_ENV_FILE:-dev.env}" ]; then
  echo "No ${DEV_ENV_FILE:-dev.env}: copy dev.env.example to dev.env and fill it in." >&2
  exit 1
fi

cargo build --release
# Environment variables override dev.env, so this forces real Globus Auth.
GLOBUS_MOCK=false exec target/release/idp_redirect_demo
