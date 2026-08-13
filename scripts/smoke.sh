#!/bin/sh
# 全栈冒烟测试：release 二进制 + 内嵌前端 + 假上游，验证一条真实请求的完整旅程。
#
# 与 Playwright E2E 的区别：E2E 用 debug 二进制验证交互流程；这个脚本验证
# **发布形态**本身 —— release 二进制能启动、前端真的被内嵌、网关真的能转发。
# 适合在发版前跑一次：sh scripts/smoke.sh
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/target/release/refract-server}"

if [ ! -x "$BIN" ]; then
  echo "binary not found: $BIN" >&2
  echo "build it first: cargo build --release -p refract-server" >&2
  exit 1
fi

GATEWAY_PORT=46820
UPSTREAM_PORT=46821
GATEWAY="http://127.0.0.1:$GATEWAY_PORT"

RUNTIME_DIR="$(mktemp -d /tmp/refract-smoke.XXXXXX)"
cleanup() {
  [ -n "$GATEWAY_PID" ] && kill "$GATEWAY_PID" 2>/dev/null || true
  [ -n "$UPSTREAM_PID" ] && kill "$UPSTREAM_PID" 2>/dev/null || true
  rm -rf "$RUNTIME_DIR"
}
trap cleanup EXIT INT TERM

echo "==> starting fake upstream on :$UPSTREAM_PORT"
python3 - "$UPSTREAM_PORT" <<'PYEOF' &
import http.server
import json
import sys

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        self.rfile.read(length)
        payload = json.dumps({
            "id": "chatcmpl-smoke",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "smoke-ok"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass

http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
PYEOF
UPSTREAM_PID=$!

echo "==> starting $BIN on :$GATEWAY_PORT"
(
  cd "$RUNTIME_DIR"
  REFRACT_LISTEN="127.0.0.1:$GATEWAY_PORT" \
  REFRACT_DATABASE="$RUNTIME_DIR/smoke.db" \
  REFRACT_REQUIRE_AUTH=false \
  exec "$BIN"
) &
GATEWAY_PID=$!

# 等网关就绪：最多 15 秒。
i=0
until curl -sf -o /dev/null "$GATEWAY/api/models"; do
  i=$((i + 1))
  if [ "$i" -gt 30 ]; then
    echo "gateway did not come up" >&2
    exit 1
  fi
  sleep 0.5
done
echo "==> gateway is up"

fail() {
  echo "SMOKE FAILED: $1" >&2
  exit 1
}

echo "==> 1. embedded frontend is served"
curl -sf "$GATEWAY/" | grep -qi "<html" || fail "index.html not served"

echo "==> 2. create a chat channel with transcode to messages"
curl -sf -X POST "$GATEWAY/api/channels" \
  -H 'content-type: application/json' \
  -d "{
    \"id\": 0, \"owner_id\": 1, \"name\": \"smoke\", \"kind\": \"chat\",
    \"enabled\": true, \"priority\": 0, \"weight\": 1,
    \"credential\": \"sk-smoke\",
    \"address\": {\"unofficial\": true, \"full_address\": false,
                  \"base_url\": \"http://127.0.0.1:$UPSTREAM_PORT\",
                  \"version_prefix\": null, \"path\": null},
    \"endpoints\": [{
      \"protocol\": \"chat\", \"order\": 0, \"enabled\": true,
      \"address\": {\"unofficial\": false, \"full_address\": false,
                    \"base_url\": null, \"version_prefix\": null, \"path\": null},
      \"credential\": null,
      \"models\": [{\"name\": \"gpt-4o\", \"upstream\": null}],
      \"transcode\": {\"enabled\": true, \"accepted\": [\"messages\"]}
    }],
    \"tags\": [], \"timeout_secs\": 0, \"proxy\": null,
    \"param_override\": null, \"note\": null
  }" > /dev/null || fail "channel creation"

echo "==> 3. model list derives from the channel"
curl -sf "$GATEWAY/v1/models" | grep -q '"gpt-4o"' || fail "gpt-4o missing from /v1/models"

echo "==> 4. native chat request passes through"
curl -sf -X POST "$GATEWAY/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "ping"}]}' \
  | grep -q '"smoke-ok"' || fail "native chat response"

echo "==> 5. messages request is transcoded to the chat upstream"
curl -sf -X POST "$GATEWAY/v1/messages" \
  -H 'content-type: application/json' \
  -d '{"model": "gpt-4o", "max_tokens": 16, "messages": [{"role": "user", "content": "ping"}]}' \
  | grep -q '"smoke-ok"' || fail "transcoded messages response"

echo "==> 6. both requests were logged"
COUNT=$(curl -sf "$GATEWAY/api/logs" | python3 -c \
  'import json,sys; d=json.load(sys.stdin)["data"]; print(sum(1 for r in d if r["status"]==200))')
[ "$COUNT" -ge 2 ] || fail "expected >=2 successful logs, got $COUNT"

echo ""
echo "SMOKE PASSED — release binary, embedded UI, routing, transcoding, logging all work."
