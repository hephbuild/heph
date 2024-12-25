package pluginsh

import (
	"connectrpc.com/connect"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/google/uuid"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	shv1 "github.com/hephbuild/hephv2/plugin/pluginsh/gen/heph/plugin/sh/v1"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

type pipe struct {
	exp  time.Time
	r    *io.PipeReader
	w    *io.PipeWriter
	busy atomic.Bool
}

type Plugin struct {
	pipes  map[string]*pipe
	pipesm sync.RWMutex
}

func (p *Plugin) PipesHandler() http.Handler {
	return PipesHandler{p}
}

func (p *Plugin) Pipe(ctx context.Context, req *connect.Request[pluginv1.PipeRequest]) (*connect.Response[pluginv1.PipeResponse], error) {
	p.pipesm.Lock()
	defer p.pipesm.Unlock()

	id := uuid.New().String()

	r, w := io.Pipe()

	p.pipes[id] = &pipe{exp: time.Now().Add(time.Minute), r: r, w: w}

	return connect.NewResponse(&pluginv1.PipeResponse{
		Path: path.Join(PipesHandlerPath, id),
		Id:   id,
	}), nil
}

func (p *Plugin) Config(ctx context.Context, c *connect.Request[pluginv1.ConfigRequest]) (*connect.Response[pluginv1.ConfigResponse], error) {
	desc := protodesc.ToDescriptorProto((&shv1.Target{}).ProtoReflect().Descriptor())

	return connect.NewResponse(&pluginv1.ConfigResponse{
		Name:         "sh",
		TargetSchema: desc,
	}), nil
}

func (p *Plugin) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	m := map[string]json.RawMessage{}
	for k, v := range req.Msg.Spec.Config {
		b, err := protojson.Marshal(v)
		if err != nil {
			return nil, err
		}
		m[k] = b
	}

	b, err := json.Marshal(m)
	if err != nil {
		return nil, err
	}

	s := &shv1.Target{}
	err = protojson.Unmarshal(b, s)
	if err != nil {
		return nil, err
	}

	var collectOutputs []*pluginv1.TargetDef_CollectOutput
	for _, output := range s.Outputs {
		paths := output.Paths
		for i, p := range paths {
			paths[i] = filepath.Join("ws", p)
		}
		collectOutputs = append(collectOutputs, &pluginv1.TargetDef_CollectOutput{
			Group: output.Group,
			Paths: paths,
		})
	}

	target, err := anypb.New(s)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{
			Ref:            req.Msg.Spec.Ref,
			Def:            target,
			Deps:           nil,
			Outputs:        nil,
			Cache:          true,
			CollectOutputs: collectOutputs,
			Codegen:        nil,
		},
	}), nil
}

func envName(prefix, group, name, value string) string {
	group = strings.ToUpper(group)
	name = strings.ToUpper(name)

	if group != "" && name != "" {
		return fmt.Sprintf("%v_%v_%v=%v", prefix, group, name, value)
	} else if group != "" {
		return fmt.Sprintf("%v_%v=%v", prefix, group, value)
	} else if name != "" {
		return fmt.Sprintf("%v_%v=%v", prefix, name, value)
	} else {
		return fmt.Sprintf("%v=%v", prefix, value)
	}
}

func (p *Plugin) Run(ctx context.Context, req *connect.Request[pluginv1.RunRequest]) (*connect.Response[pluginv1.RunResponse], error) {
	for len(req.Msg.Pipes) < 3 {
		req.Msg.Pipes = append(req.Msg.Pipes, "")
	}

	var t shv1.Target
	err := req.Msg.Target.Def.UnmarshalTo(&t)
	if err != nil {
		return nil, err
	}

	workdir := filepath.Join(req.Msg.SandboxPath, "ws")

	err = os.MkdirAll(workdir, 0755)
	if err != nil {
		return nil, err
	}

	var env []string

	for _, input := range req.Msg.Inputs {
		// TODO: expand based on encoding
		path := input.Name // TODO: make it a path

		env = append(env, envName("SRC", input.Group, input.Name, path))
	}
	for _, output := range t.Outputs {
		path := output.Name // TODO: make it a path

		env = append(env, envName("OUT", output.Group, output.Name, path))
	}

	var stdoutWriters, stderrWriters []io.Writer

	logFile, err := os.Create(filepath.Join(req.Msg.SandboxPath, "log.txt"))
	if err != nil {
		return nil, err
	}
	defer logFile.Close()

	stdoutWriters = append(stdoutWriters, logFile)
	stderrWriters = append(stderrWriters, logFile)

	for i, id := range req.Msg.Pipes[1:3] {
		if id == "" {
			continue
		}

		pipe, ok := p.getPipe(id)
		if !ok {
			return nil, errors.New("pipe 0 not found")
		}

		defer p.removePipe(id)

		if i == 0 {
			stdoutWriters = append(stdoutWriters, pipe.w)
		} else {
			stderrWriters = append(stderrWriters, pipe.w)
		}
	}

	cmd := exec.CommandContext(ctx, t.Run[0], t.Run[1:]...)
	cmd.Env = env
	cmd.Dir = workdir

	// TODO: pty
	if id := req.Msg.Pipes[0]; id != "" {
		pipe, ok := p.getPipe(id)
		if !ok {
			return nil, errors.New("pipe 0 not found")
		}
		cmd.Stdin = pipe.r
	}
	cmd.Stdout = io.MultiWriter(stdoutWriters...)
	cmd.Stderr = io.MultiWriter(stderrWriters...)

	// TODO: kill all children

	err = cmd.Run()
	if err != nil {
		return nil, err
	}

	stat, err := logFile.Stat()
	if err != nil {
		return nil, err
	}

	err = logFile.Close()
	if err != nil {
		return nil, err
	}

	var artifacts []*pluginv1.Artifact
	if stat.Size() > 0 {
		artifacts = append(artifacts, &pluginv1.Artifact{
			Name: "log.txt",
			Type: pluginv1.Artifact_TYPE_LOG,
			Uri:  "file://" + logFile.Name(),
		})
	}

	return connect.NewResponse(&pluginv1.RunResponse{
		Artifacts: artifacts,
	}), nil
}

func New() *Plugin {
	return &Plugin{
		pipes: map[string]*pipe{},
	}
}

var _ pluginv1connect.DriverHandler = (*Plugin)(nil)
