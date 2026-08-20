#!/bin/sh
set -eu
cd /workspace
if curl -sf -o /dev/null --max-time 2 http://127.0.0.1:3940/; then
  exit 0
fi
pnpm --filter @refract/homepage dev >>/tmp/app-startup.log 2>&1 &
