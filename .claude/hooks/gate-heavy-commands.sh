#!/usr/bin/env bash
# PreToolUse(Bash) gate: refuses the two commands CLAUDE.md says not to run
# reflexively, and foreground sleeps used to poll.
#
#   tst, e2e    CI runs both on every push. Across ten recent sessions they
#               ran 30 and 132 times locally anyway — `e2e` is a full release
#               build each time. The instruction alone did not hold, so this
#               makes the exception explicit: prefix `HEPH_FULL_SUITE=1` when
#               the change really warrants it (see CLAUDE.md "Workflow").
#   sleep >=30  Polling CI with sleep burned ~5h of wall-clock over the same
#               sessions. Wait on `gh run watch` in the background instead.
#
# Exit 2 hands stderr back to the agent as the reason. Anything this script
# cannot parse is allowed: a gate that misfires on a legitimate command costs
# more than one that occasionally lets a suite run.
#
# Portable to macOS's /bin/bash 3.2 and BSD userland: no sed/grep, no mapfile,
# no associative arrays. Tested by tests/claude_hooks.rs.

cmd=$(jq -r '.tool_input.command // empty' 2>/dev/null) || exit 0
[ -n "$cmd" ] || exit 0

block() {
  printf '%s\n' "$1" >&2
  exit 2
}

nl=$'\n'

# Drop heredoc bodies: they are data — a file being written, a commit message,
# a script piped to python — and markdown in them is full of `tst`/`e2e`.
heredoc_re="(^|[^<])<<-?[[:space:]]*['\"]?([A-Za-z_][A-Za-z0-9_]*)"
code=
term=
while IFS= read -r line; do
  if [ -n "$term" ]; then
    [ "${line#"${line%%[![:space:]]*}"}" = "$term" ] && term=
    continue
  fi
  code+="$line$nl"
  [[ $line =~ $heredoc_re ]] && term=${BASH_REMATCH[2]}
done <<<"$cmd"

# Split into simple commands. Crude on purpose — quoting is ignored, so a
# `tst` inside a string can be read as a command; the escape hatch covers it.
# Backticks are not split on: prose in a -m message uses them far more often
# than a command substitution runs a suite.
for sep in '&&' '||' ';' '|' '&' '$(' '(' ')' '{' '}'; do
  code=${code//"$sep"/$nl}
done

while IFS= read -r seg; do
  allow=0
  # Peel env assignments and wrappers that still run the next word.
  while :; do
    seg="${seg#"${seg%%[![:space:]]*}"}"
    if [[ $seg =~ ^([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*)([[:space:]]+(.*))?$ ]]; then
      [ "${BASH_REMATCH[1]}" = "HEPH_FULL_SUITE=1" ] && allow=1
      seg=${BASH_REMATCH[3]}
    elif [[ $seg =~ ^(time|nohup|command|exec|env|do|then|else|--)([[:space:]]+(.*))?$ ]]; then
      seg=${BASH_REMATCH[3]}
    elif [[ $seg =~ ^timeout[[:space:]]+[^[:space:]]+([[:space:]]+(.*))?$ ]]; then
      seg=${BASH_REMATCH[2]}
    elif [[ $seg =~ ^devenv[[:space:]]+shell([[:space:]]+(.*))?$ ]]; then
      seg=${BASH_REMATCH[2]}
    else
      break
    fi
  done

  word=${seg%%[[:space:]]*}
  case $word in
    tst)
      [ $allow = 1 ] || block "Blocked: \`tst\` is the full suite, and CI runs it on every push.
Run the tests for what you changed: \`cargo test -p <crate> <name>\`, then push.
Only a large blast-radius change (engine core, provider/driver traits, caching) runs it locally, before opening the PR —
if that is this change, re-run as \`HEPH_FULL_SUITE=1 tst\`. See CLAUDE.md \"Workflow\"."
      ;;
    e2e)
      [ $allow = 1 ] || block "Blocked: \`e2e\` does a full --release build, and the bin_e2e CI job runs it on all three platforms on every push.
Run it locally only when changing what it covers — the plugin loader, the TUI, CLI exit codes, or the e2e script itself.
If that is this change, re-run as \`HEPH_FULL_SUITE=1 e2e ...\` (and check the \`running N tests\` count). See CLAUDE.md \"e2e\"."
      ;;
    sleep)
      n=${seg#sleep}
      n=${n//[[:space:]]/}
      n=${n%s}
      if [[ $n =~ ^[0-9]+$ ]] && [ "$n" -ge 30 ]; then
        block "Blocked: foreground \`sleep $n\`. Don't poll.
Waiting on CI: \`gh run watch <run-id> --exit-status\` with run_in_background — the session is woken when it exits.
Waiting on a local process: run it with run_in_background, or use Monitor with an until-loop."
      fi
      ;;
  esac
done <<<"$code"

exit 0
