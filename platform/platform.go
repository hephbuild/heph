package platform

import (
	"context"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/targetspec"
)

type Provider interface {
	NewExecutor(labels map[string]string, options map[string]interface{}) (Executor, error)
}

type ExecOptions struct {
	WorkDir  string
	BinDir   string
	HomeDir  string
	Target   targetspec.TargetSpec
	Env      map[string]string
	Run      []string
	TermArgs []string
	IOCfg    sandbox.IOConfig
}

type Executor interface {
	Exec(ctx context.Context, o ExecOptions, execArgs []string) error
	Os() string
	Arch() string
}
