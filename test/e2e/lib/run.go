package lib

import (
	"bytes"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
)

type RunOpts struct {
	Params map[string]string
}

func (o RunOpts) Args() []string {
	var args []string
	for k, v := range o.Params {
		args = append(args, "-p", k+"="+v)
	}
	return args
}

var defaultOpts RunOpts

func SetDefaultRunOpts(o RunOpts) {
	defaultOpts = o
}

func Run(tgt string) error {
	return RunO(tgt, defaultOpts)
}

func RunO(tgt string, o RunOpts) error {
	args := []string{"run", tgt}
	args = append(args, o.Args()...)

	cmd := exec.Command("heph", args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	return cmd.Run()
}

func RunOutput(tgt string) ([]string, error) {
	var stdout, stderr bytes.Buffer
	cmd := exec.Command("heph", "q", "out", tgt)
	cmd.Stderr = io.MultiWriter(os.Stderr, &stderr)
	cmd.Stdout = &stdout

	err := cmd.Run()
	if err != nil {
		return nil, fmt.Errorf("%v: %v", err, stderr.String())
	}

	s := strings.TrimSpace(stdout.String())
	if len(s) == 0 {
		return nil, nil
	}

	outputs := strings.Split(s, "\n")

	for _, output := range outputs {
		if !PathExists(output) {
			return outputs, fmt.Errorf("%v doesnt exist", output)
		}
	}

	return outputs, nil
}
