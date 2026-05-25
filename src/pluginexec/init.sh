set -uEo pipefail
export PS1='$ '
export BASH_SILENCE_DEPRECATION_WARNING=1

echo "             _          _ _ "
echo "            | |        | | |"
echo "   )     ___| |__   ___| | |"
echo "  ) \   / __| '_ \ / _ \ | |"
echo " / ) (  \__ \ | | |  __/ | |"
echo " \(_)/  |___/_| |_|\___|_|_|"
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
