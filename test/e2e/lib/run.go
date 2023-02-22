package lib

import (
	"bytes"
	"fmt"
	"io"
	"os"
	"strings"
)

func Run(tgt string) error {
	return RunO(tgt, defaultOpts)
}

func RunO(tgt string, o RunOpts) error {
	cmd := commandO(o, "run", tgt)
	if !o.Silent {
		cmd.Stdout = os.Stdout
		cmd.Stderr = os.Stderr
	}
	if o.Shell {
		cmd.Stdin = os.Stdin
	}

	return cmd.Run()
}

func RunOutput(tgt string) ([]string, error) {
	return RunOutputO(tgt, defaultOpts)
}

func RunOutputO(tgt string, o RunOpts) ([]string, error) {
	var stdout, stderr bytes.Buffer
	cmd := commandO(o, "run", tgt, "--print-out")
	cmd.Stderr = io.MultiWriter(os.Stderr, &stderr)
	cmd.Stdout = &stdout
	if len(o.Env) > 0 {
		cmd.Env = os.Environ()
		for k, v := range o.Env {
			cmd.Env = append(cmd.Env, k+"="+v)
		}
	}

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
