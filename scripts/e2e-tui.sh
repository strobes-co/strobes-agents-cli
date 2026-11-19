#!/usr/bin/env bash
# End-to-end check of the chat UX against a live Strobes API.
#
# Drives the real binary inside tmux and asserts on what is on screen, so the
# Claude Code-style behaviours (launch flags, / ! @ ? menus, history, paste,
# tool rendering, interrupt, quit, print mode) are checked the way a user
# meets them. Needs tmux and a configured tenant:
#
#   scripts/e2e-tui.sh                 # tenant "local", release binary
#   TENANT=staging BIN=target/debug/strobes scripts/e2e-tui.sh
#   MODEL=40 scripts/e2e-tui.sh        # model id (default 46 = SAI)
#
# Agent turns hit the real model, so the run takes a few minutes.
set -u
TENANT=${TENANT:-local}
BIN=${BIN:-target/release/strobes}
MODEL=${MODEL:-46}
S=strobes-e2e
PASS=0
FAIL=0
export LANG=${LANG:-en_US.UTF-8}

[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 2; }
command -v tmux >/dev/null || { echo "tmux is required"; exit 2; }

start() { # start [cols rows] -- args...
  local cols=$1 rows=$2; shift 2
  tmux kill-session -t $S 2>/dev/null
  tmux new-session -d -s $S -x "$cols" -y "$rows" "env LANG=$LANG $BIN --tenant $TENANT -m $MODEL $*; sleep 3"
}
screen() { tmux capture-pane -t $S -p; }
keys() { tmux send-keys -t $S "$@"; }
# wait_for PATTERN SECS — poll the screen for an extended regex.
wait_for() {
  local end=$((SECONDS + $2))
  while [ $SECONDS -lt $end ]; do
    screen | grep -Eq -- "$1" && return 0
    sleep 0.3
  done
  return 1
}
ok() { PASS=$((PASS + 1)); printf '  \033[32m✔\033[0m %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  \033[31m✘\033[0m %s\n' "$1"; screen | sed 's/^/      │ /' | tail -15; }
check() { if wait_for "$2" "${3:-8}"; then ok "$1"; else bad "$1"; fi; }
count() { screen | grep -Ec -- "$1"; }
# wait_idle SECS — the agent has finished (no "esc to interrupt" activity line).
wait_idle() {
  local end=$((SECONDS + $1))
  while [ $SECONDS -lt $end ]; do
    screen | grep -q "esc to interrupt)" || return 0
    sleep 0.5
  done
  return 1
}

echo "▸ launch"
start 100 32
check "bare launch opens a prompt (no pickers)" "Ask anything" 10
check "welcome names the product" "Strobes Agents"
screen | grep -q "Select a workspace" && bad "no workspace picker on launch" || ok "no workspace picker on launch"

echo "▸ menus"
keys "?"; check "? shows shortcuts" "search prompt history"
keys Escape; sleep 0.3
keys "/"; check "/ opens the command menu with built-ins first" "/help +Show commands"
keys "hel"; sleep 0.2; keys Enter; check "/help prints commands and shortcuts" "^ *Shortcuts"
keys "!echo e2e-bash-ok" Enter; check "!cmd runs locally" "Bash\(echo e2e-bash-ok\)" 10
check "!cmd output under its call" "⎿  e2e-bash-ok"
keys Up; check "↑ recalls the last prompt" "> !echo e2e-bash-ok"
keys C-c; sleep 0.2
keys C-r; check "Ctrl-R searches history" "Search prompt history"
keys Escape; sleep 0.2
keys "look at @READM"; check "@ lists files" "README.md"
keys C-c; sleep 0.2
seq 1 20 | tmux load-buffer -; tmux paste-buffer -p -t $S
check "long paste collapses" "\[Pasted text #1 \+20 lines\]"
keys C-c; sleep 0.2

echo "▸ agent turns"
keys "reply with exactly: e2e-pong-7731" Enter
check "agent reply streams in" "⏺ .*e2e-pong-7731" 90
if wait_idle 60; then ok "activity line clears when done"; else bad "activity line clears when done"; fi
n=$(count "^ ⏺ .*e2e-pong-7731"); [ "$n" = 1 ] && ok "reply shown once" || bad "reply shown once (got $n)"
keys "run the shell command: echo e2e-shell-4242" Enter
check "tool call renders as Shell(…)" "Shell\(.*e2e-shell-4242" 90
check "tool output under ⎿" "⎿  e2e-shell-4242" 30
wait_idle 90
n=$(count "Shell\(.*e2e-shell-4242"); [ "$n" = 1 ] && ok "one line per tool call" || bad "one line per tool call (got $n)"
keys "write a 600 word essay about the history of TLS" Enter
check "agent starts working" "esc to interrupt\)" 30
keys Escape
check "Esc interrupts" "Interrupted" 30

echo "▸ sub-agents"
wait_idle 60
keys "Delegate to a sub-agent (not yourself): have it run the shell command 'echo e2e-sub-5150' and report back. Then reply in one line." Enter
check "sub-agent gets its own group" "sub-agent$" 180
check "its tool calls are listed in the group" "⎿  Shell\(echo e2e-sub-5150" 180
wait_idle 240
check "group settles to Done" "Done \([0-9]+ tool use" 30
# The sub-agent's prose stays out of the main transcript (collapsed).
keys C-o; sleep 0.5
check "ctrl+o expands the sub-agent transcript" "^     +⏺ " 5
keys C-o; sleep 0.3

echo "▸ quit"
wait_idle 30
keys C-c; check "first Ctrl-C asks to confirm" "Press Ctrl-C again to exit" 3
keys C-c; sleep 1
tmux list-panes -t $S -F '#{pane_current_command}' 2>/dev/null | grep -q strobes && bad "second Ctrl-C quits" || ok "second Ctrl-C quits"

echo "▸ resume"
start 100 32 -c
check "-c reopens the last chat" "e2e-sub-5150" 15
keys C-c C-c
start 100 32 -r
check "-r opens the resume list" "Resume a chat" 15
keys Escape; sleep 0.3; keys C-c C-c

echo "▸ narrow terminal"
start 60 24
check "narrow placeholder" "Ask anything…" 10
w=$(screen | python3 -c 'import sys; print(max((len(l.rstrip("\n")) for l in sys.stdin), default=0))')
[ "${w:-99}" -le 60 ] && ok "nothing overflows 60 columns" || bad "nothing overflows 60 columns ($w)"
keys C-c C-c

echo "▸ non-interactive"
out=$(timeout 120 "$BIN" --tenant "$TENANT" -m "$MODEL" -p "reply with exactly: e2e-print-ok" 2>/dev/null)
[ "$(echo "$out" | tr -d '[:space:]')" = "e2e-print-ok" ] && ok "-p prints only the answer" || bad "-p prints only the answer (got: $out)"
"$BIN" --tenant "$TENANT" status >/dev/null 2>&1 && ok "--tenant X <subcommand> still runs it" || bad "--tenant X <subcommand> still runs it"
"$BIN" --tenant "$TENANT" -p 2>&1 </dev/null | grep -q "needs a prompt" && ok "-p without a prompt explains itself" || bad "-p without a prompt explains itself"

tmux kill-session -t $S 2>/dev/null
echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
