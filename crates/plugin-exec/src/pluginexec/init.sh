set -uEo pipefail
export PS1='$ '
export BASH_SILENCE_DEPRECATION_WARNING=1

__heph_render_banner() {
  # Color codes via ANSI-C quoting ($'…' interprets \e regardless of echo/xpg
  # settings); printf '%s\n' emits the art verbatim, never mangling backslashes.
  local W=$'\e[38;2;232;234;237m'
  local B=$'\e[38;2;45;91;255m'
  local X=$'\e[0m'

  printf '%s\n' "                   _          _ _ "
  printf '%s\n' " ${W}    ▓▒${B}   ▓▒${X}      | |        | | |"
  printf '%s\n' " ${W}   ▓▒${B}   ▓▒${X}    ___| |__   ___| | |"
  printf '%s\n' " ${W}  ▓▒${B}   ▓▒${X}    / __| '_ \ / _ \ | |"
  printf '%s\n' " ${W} ▓▒${B}   ▓▒${X}     \__ \ | | |  __/ | |"
  printf '%s\n' " ${W}▓▒${B}   ▓▒${X}      |___/_| |_|\___|_|_|"
}

__heph_render_banner
echo ''
echo "=========== Help ==========="
echo 'Shell mode, to exit; run exit or ctrl-d'
{% if entrypoint %}
# The caller's args are this shell's positional parameters, copied at top level
# because run and xrun are functions with their own "$@". An interactive bash
# binds them only after reading its rcfile, so they are taken again just before
# the first prompt, which then steps aside for any PROMPT_COMMAND of the
# caller's. (`${1+"$@"}`: bash 3.2's `set -u` trips on a bare "$@".)
__heph_args=(${1+"$@"})
__heph_prompt_command=${PROMPT_COMMAND-}
PROMPT_COMMAND='__heph_args=(${1+"$@"}); PROMPT_COMMAND=$__heph_prompt_command; showl'
# Each helper runs the command with its own args when given any, the ones
# `heph run` was given otherwise. A bare call always means the originals.
# (The `+` form keeps an empty array from tripping `set -u` on bash 3.2.)
run() (
  set -euo pipefail
  if [ "$#" -gt 0 ]; then __heph_a=("$@"); else __heph_a=(${__heph_args[@]+"${__heph_args[@]}"}); fi
  {{ cmds }} ${__heph_a[@]+"${__heph_a[@]}"}
)
xrun() (
  set -euo pipefail
  if [ "$#" -gt 0 ]; then __heph_a=("$@"); else __heph_a=(${__heph_args[@]+"${__heph_args[@]}"}); fi
  set -x
  {{ cmds }} ${__heph_a[@]+"${__heph_a[@]}"}
)
show() {
  printf '%s' {{ display }}
  if [ "$#" -gt 0 ]; then printf ' %q' "$@"; elif [ -n "${__heph_args[*]+set}" ]; then printf ' %q' "${__heph_args[@]}"; fi
  echo
}
showl() {
  printf '1 │ '
  show "$@"
}

echo "run [args…]   : Runs the command; args, when given, replace the ones heph run got"
echo "xrun [args…]  : Same with trace"
echo "show [args…]  : Prints the command, args shell-quoted"
echo "showl [args…] : Same with line numbers"
echo ''
echo "============ Run ==========="
{% elif cmds %}
# Commands as an array (one element per line) so the runN helpers can replay a
# prefix without re-emitting every line
__heph_cmds=(
{% for line in lines %}  {{ line.quoted }}
{% endfor %})
# Run the first $1 commands; pass "x" as $2 to trace (set -x).
__heph_run_upto() (
  set -euo pipefail
  if [ "${2:-}" = x ]; then set -x; fi
  __heph_n=$1
  __heph_i=0
  for __heph_c in "${__heph_cmds[@]}"; do
    __heph_i=$((__heph_i + 1))
    [ "$__heph_i" -le "$__heph_n" ] || break
    eval "$__heph_c"
  done
)
run() (
  set -euo pipefail
  {{ cmds }}
)
xrun() (
  set -euxo pipefail
  {{ cmds }}
)
{% for line in lines %}run{{ line.n }}() { __heph_run_upto {{ line.n }}; }
xrun{{ line.n }}() { __heph_run_upto {{ line.n }} x; }
{% endfor %}show() {
  cat << 'HEPH_EOF'
{{ cmds }}
HEPH_EOF
}
showl() {
  cat << 'HEPH_EOF'
{% for line in lines %}{{ line.label }} │ {{ line.text }}
{% endfor %}HEPH_EOF
}

echo "run   : Runs the commands"
echo "xrun  : Runs the commands with trace"
echo "runN  : Runs the commands up to line N (run1..run{{ lines|length }})"
echo "xrunN : Same with trace"
echo "show  : Prints the commands"
echo "showl : Prints the commands with line numbers"
echo ''
echo "============ Run ==========="
showl
{% endif %}
echo ''
