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
run() (
  set -euo pipefail
  {{ cmds }}
)
xrun() (
  set -euxo pipefail
  {{ cmds }}
)
show() {
  cat << 'HEPH_EOF'
{{ cmds }}
HEPH_EOF
}

echo "run   : Runs the commands"
echo "xrun  : Runs the commands with trace"
echo "show  : Prints the commands"
echo ''
echo "============ Run ==========="
show
{% endif %}
echo ''
