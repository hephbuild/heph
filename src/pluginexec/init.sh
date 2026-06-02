set -uEo pipefail
export PS1='$ '
export BASH_SILENCE_DEPRECATION_WARNING=1

echo -e "                   _          _ _ "
echo -e " \e[38;2;232;234;237m    ▓▒\e[38;2;45;91;255m   ▓▒\e[0m      | |        | | |"
echo -e " \e[38;2;232;234;237m   ▓▒\e[38;2;45;91;255m   ▓▒\e[0m    ___| |__   ___| | |"
echo -e " \e[38;2;232;234;237m  ▓▒\e[38;2;45;91;255m   ▓▒\e[0m    / __| '_ \ / _ \ | |"
echo -e " \e[38;2;232;234;237m ▓▒\e[38;2;45;91;255m   ▓▒\e[0m     \__ \ | | |  __/ | |"
echo -e " \e[38;2;232;234;237m▓▒\e[38;2;45;91;255m   ▓▒\e[0m      |___/_| |_|\___|_|_|"
echo ''
echo "=========== Help ==========="
echo 'Shell mode, to exit; run exit or ctrl-d'

{{@if cmds exists}}
run() (
  set -euo pipefail
  {{cmds}}
)
xrun() (
  set -euxo pipefail
  {{cmds}}
)
show() {
  cat << 'HEPH_EOF'
{{cmds}}
HEPH_EOF
}

echo "run   : Runs the commands"
echo "xrun  : Runs the commands with trace"
echo "show  : Prints the commands"
echo ''
echo "============ Run ==========="
show
{{@endif}}
echo ''
