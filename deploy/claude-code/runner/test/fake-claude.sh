#!/bin/sh
# Stands in for `claude -p` in tests. The prompt arrives on stdin, as with
# the real runner. Behaviour by prompt: "fail", "sleep", "nocommit",
# "nochange", "opus-sub" (a subagent on Opus), otherwise a normal run that
# commits a file. Logs its arguments and the security-relevant environment
# to $FAKE_LOG if set.
prompt="$(cat)"
if [ -n "$FAKE_LOG" ]; then
  printf '%s | sub=%s opus_alias=%s token=%s runner_token_file=%s home=%s\n' "$*" \
    "${CLAUDE_CODE_SUBAGENT_MODEL:-}" "${ANTHROPIC_DEFAULT_OPUS_MODEL:-}" \
    "${CLAUDE_CODE_OAUTH_TOKEN:+set}" "${RUNNER_TOKEN_FILE:-}" "$HOME" >> "$FAKE_LOG"
fi
ok() { printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","total_cost_usd":0.02,"num_turns":4,"modelUsage":{"%s":{}}}\n' "$1" "${2:-claude-sonnet-5-5}"; }
case "$prompt" in
  fail)
    echo '{"type":"result","subtype":"error_max_budget_usd","is_error":true,"result":"","total_cost_usd":0.5,"num_turns":3}'
    exit 1 ;;
  sleep)
    sleep 30 ;;
  nocommit)
    echo "zmiana" > niezacommitowane.txt
    ok "zrobione bez commita" ;;
  nochange)
    ok "nic nie zmieniłem" ;;
  opus-sub)
    echo "x" > wynik.txt && git add wynik.txt && git commit -q -m x
    printf '{"type":"result","subtype":"success","is_error":false,"result":"ok","total_cost_usd":0.4,"num_turns":4,"modelUsage":{"claude-sonnet-5-5":{},"claude-opus-5-5":{}}}\n' ;;
  -*)
    echo "plik" > wynik.txt && git add wynik.txt && git commit -q -m "brief z myślnikiem"
    ok "brief z myślnikiem dotarł jako tekst" ;;
  *)
    echo "plik od claude" > wynik.txt
    git add wynik.txt && git commit -q -m "claude: wynik" || exit 3
    ok "gotowe" ;;
esac
