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
{% if cmds %}
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
