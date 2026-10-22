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

# The .cmd runs the Windows .exe; under WSL build and run the Linux binary.
cargo build --release
exec target/release/idp_redirect_demo
