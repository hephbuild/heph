package pluginsh

import (
	"connectrpc.com/connect"
	"context"
	"encoding/json"
	"fmt"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	shv1 "github.com/hephbuild/hephv2/plugin/pluginsh/gen/heph/plugin/sh/v1"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/types/known/anypb"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

type Plugin struct {
}

func (p Plugin) Config(ctx context.Context, c *connect.Request[pluginv1.ConfigRequest]) (*connect.Response[pluginv1.ConfigResponse], error) {
	desc := protodesc.ToDescriptorProto((&shv1.Target{}).ProtoReflect().Descriptor())

	return connect.NewResponse(&pluginv1.ConfigResponse{
		Name:         "sh",
		TargetSchema: desc,
	}), nil
}

func (p Plugin) Parse(ctx context.Context, req *connect.Request[pluginv1.ParseRequest]) (*connect.Response[pluginv1.ParseResponse], error) {
	m := map[string]json.RawMessage{}
	for k, v := range req.Msg.Config {
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

	target, err := anypb.New(s)
	if err != nil {
		return nil, err
	}

	return connect.NewResponse(&pluginv1.ParseResponse{
		Target: &pluginv1.TargetDef{Def: target},
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

func (p Plugin) Run(ctx context.Context, req *connect.Request[pluginv1.RunRequest]) (*connect.Response[pluginv1.RunResponse], error) {
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

	logFile, err := os.Create(filepath.Join(req.Msg.SandboxPath, "log.txt"))
	if err != nil {
		return nil, err
	}
	defer logFile.Close()

	cmd := exec.CommandContext(ctx, t.Run[0], t.Run[1:]...)
	cmd.Env = env
	cmd.Dir = workdir

	// TODO: pty
	//cmd.Stdin = os.DevNull
	cmd.Stdout = io.MultiWriter(logFile)
	cmd.Stderr = io.MultiWriter(logFile)

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
			Name:      "log.txt",
			WellKnown: pluginv1.Artifact_WELL_KNOWN_LOG,
			Uri:       "file://" + logFile.Name(),
		})
	}

	return connect.NewResponse(&pluginv1.RunResponse{
		Artifacts: artifacts,
	}), nil
}

func New() *Plugin {
	return &Plugin{}
}

var _ pluginv1connect.DriverHandler = (*Plugin)(nil)
