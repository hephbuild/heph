set -u
export PS1='$ '
export BASH_SILENCE_DEPRECATION_WARNING=1

{{ if .Cmds }}
run() {
	{{.Cmds}}
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
echo "show  : Prints the commands"
echo ''
echo "============ Run ==========="
show
{{end}}
echo ''
