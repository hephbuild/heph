package platform

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/utils/xcontext"
	"runtime"
)

func IsLocalExecutor(p Executor) bool {
	_, ok := p.(*localExecutor)
	return ok
}

func HasHostFsAccess(p Executor) bool {
	return IsLocalExecutor(p)
}

type localExecutor struct{}

func (p *localExecutor) Os() string {
	return runtime.GOOS
}

func (p *localExecutor) Arch() string {
	return runtime.GOARCH
}

func (p *localExecutor) Exec(ctx context.Context, o ExecOptions, execArgs []string) error {
	env := o.Env

	exPath, err := sandbox.LookPath(execArgs[0], o.Env["PATH"])
	if err != nil {
		return fmt.Errorf("local: %w", err)
	}
	execArgs[0] = exPath

	log.Debugf("exec: %v", execArgs)

	err = sandbox.FilterLongEnv(env, execArgs)
	if err != nil {
		return err
	}

	sctx, hctx, cancel := xcontext.NewSoftCancel(ctx)
	defer cancel()

	cmd := sandbox.Exec(sandbox.ExecConfig{
		Context:     hctx,
		SoftContext: sctx,
		BinDir:      o.BinDir,
		Dir:         o.WorkDir,
		Env:         env,
		IOConfig:    o.IOCfg,
		ExecArgs:    execArgs,
	})

	err = cmd.Run()
	if err != nil {
		return err
	}

	return nil
}

func NewLocalExecutor() Executor {
	return &localExecutor{}
}

type localProvider struct {
	labels map[string]string
}

func (p *localProvider) NewExecutor(labels map[string]string, _ map[string]interface{}) (Executor, error) {
	if !HasAllLabels(labels, p.labels) {
		return nil, nil
	}

	return NewLocalExecutor(), nil
}

func NewLocalProvider(name string) Provider {
	return &localProvider{
		labels: map[string]string{
			"name": name,
			"os":   runtime.GOOS,
			"arch": runtime.GOARCH,
		},
	}
}

func init() {
	RegisterProvider("local", func(name string, options map[string]interface{}) (Provider, error) {
		return NewLocalProvider(name), nil
	})
}
