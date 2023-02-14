package lib

import (
	"os"
	"os/exec"
)

type RunOpts struct {
	Params   map[string]string
	LogLevel string
	Shell    bool
}

func (o RunOpts) Args() []string {
	var args []string
	for k, v := range o.Params {
		args = append(args, "-p", k+"="+v)
	}
	if o.LogLevel != "" {
		args = append(args, "--log_level="+o.LogLevel)
	}
	if o.Shell {
		args = append(args, "--shell")
	}
	return args
}

var defaultOpts RunOpts

func setupDefaultOpts() {
	if lvl := os.Getenv("LOG"); lvl != "" {
		defaultOpts.LogLevel = lvl
	}
}

func init() {
	setupDefaultOpts()
}

func SetDefaultRunOpts(o RunOpts) {
	defaultOpts = o
	setupDefaultOpts()
}

func command(args ...string) *exec.Cmd {
	return commandO(defaultOpts, args...)
}

func commandO(o RunOpts, args ...string) *exec.Cmd {
	// TODO handle cli args
	args = append(o.Args(), args...)

	return exec.Command("heph", args...)
}
