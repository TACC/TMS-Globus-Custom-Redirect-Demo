#!/usr/bin/env bash
# Ubuntu / WSL2 counterpart of run_real_globus.cmd: run the demo against real Globus Auth.
set -euo pipefail
cd "$(dirname "$0")"

export GLOBUS_CLIENT_ID=2cc0a4ef-235b-4f90-b754-843df1e3c021
# GLOBUS_CLIENT_SECRET comes from the environment; it is never stored here.
if [ -z "${GLOBUS_CLIENT_SECRET:-}" ]; then
  echo 'GLOBUS_CLIENT_SECRET is not set. Set it first, e.g.: export GLOBUS_CLIENT_SECRET="..."' >&2
  exit 1
fi
export GLOBUS_CLIENT_SECRET
unset GLOBUS_MOCK

# TaccTapis login uses the TMS-prototyping dev client. Only its TAPIS_* entries
# are read (the file is not sourced); values already in the environment win.
TAPIS_ENV_FILE=${TAPIS_ENV_FILE:-../../TMS-prototyping/dev.env}
if [ -f "$TAPIS_ENV_FILE" ]; then
  env_value() { sed -n "s/^$1=//p" "$TAPIS_ENV_FILE" | tail -n 1; }
  export TAPIS_BASE_URL=${TAPIS_BASE_URL:-$(env_value TAPIS_TENANT_URL)}
  export TAPIS_CLIENT_ID=${TAPIS_CLIENT_ID:-$(env_value TAPIS_OAUTH_CLIENT_ID)}
  export TAPIS_CLIENT_SECRET=${TAPIS_CLIENT_SECRET:-$(env_value TAPIS_OAUTH_CLIENT_KEY)}
  export TAPIS_CALLBACK_URL=${TAPIS_CALLBACK_URL:-$(env_value TAPIS_OAUTH_CALLBACK_URL)}
else
  echo "No $TAPIS_ENV_FILE; TaccTapis login stays disabled unless TAPIS_CLIENT_ID/SECRET are set." >&2
fi

# The .cmd runs the Windows .exe; under WSL build and run the Linux binary.
cargo build --release
exec target/release/idp_redirect_demo
