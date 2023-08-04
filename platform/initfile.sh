set -uEo pipefail
export PS1='$ '
export BASH_SILENCE_DEPRECATION_WARNING=1

{{ if .Cmds }}
trace-off() {
  { set +x; } 2>/dev/null
}

run() {
	{{.Cmds}}
}
xrun() {
  trap trace-off ERR
  set -x
  {{.Cmds}}
  trace-off
}
show() {
	echo '{{.CmdsStr}}'
}
{{end}}

echo "             _          _ _ "
echo "            | |        | | |"
echo "   )     ___| |__   ___| | |"
echo "  ) \   / __| '_ \ / _ \ | |"
echo " / ) (  \__ \ | | |  __/ | |"
echo " \(_)/  |___/_| |_|\___|_|_|"
echo ''
echo "=========== Help ==========="
echo 'Shell mode, to exit; run exit or ctrl-d'
{{ if .Cmds }}
echo "run   : Runs the commands"
echo "xrun  : Runs the commands with trace"
echo "show  : Prints the commands"
echo ''
echo "============ Run ==========="
show
{{end}}
echo ''
