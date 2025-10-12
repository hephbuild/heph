package pluginexec

import (
	"context"
	"strings"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/structpb"
)

type Option[S proto.Message] func(*Plugin[S])

func WithRunToExecArgs[S proto.Message](f RunToExecArgsFunc[S]) Option[S] {
	return func(plugin *Plugin[S]) {
		plugin.runToExecArgs = f
	}
}

func WithName[S proto.Message](name string) Option[S] {
	return func(plugin *Plugin[S]) {
		plugin.name = name
	}
}

func WithDefaultLinuxPath[S proto.Message]() Option[S] {
	return WithPath[S]([]string{
		"/usr/local/bin",
		"/usr/bin",
		"/bin",
	})
}

func WithPath[S proto.Message](path []string) Option[S] {
	return func(plugin *Plugin[S]) {
		plugin.path = path
		plugin.pathStr = strings.Join(path, ":")
	}
}

const NameExec = "exec"

func New[S proto.Message](
	name string,
	getTarget func(S) *execv1.Target,
	parseConfig func(ctx context.Context, ref *pluginv1.TargetRef, config map[string]*structpb.Value) (*pluginv1.TargetDef, error),
	runToExecArgs RunToExecArgsFunc[S],
	options ...Option[S],
) *Plugin[S] {
	p := &Plugin[S]{
		pipes:         map[string]*pipe{},
		name:          name,
		getExecTarget: getTarget,
		parseConfig:   parseConfig,
		runToExecArgs: runToExecArgs,
	}
	WithDefaultLinuxPath[S]()(p)

	for _, opt := range options {
		opt(p)
	}

	return p
}
