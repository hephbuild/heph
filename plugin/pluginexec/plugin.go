package pluginexec

import (
	"context"
	"io"
	"net/http"
	"path"
	"sync"
	"sync/atomic"
	"time"

	"github.com/hephbuild/heph/internal/hproto"
	"github.com/hephbuild/heph/internal/htypes"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/structpb"

	"github.com/google/uuid"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	"google.golang.org/protobuf/reflect/protodesc"
)

type pipe struct {
	exp  time.Time
	r    *io.PipeReader
	w    *io.PipeWriter
	busy atomic.Bool
}

type RunToExecArgsFunc[T any] = func(sandboxPath string, t T, termargs []string) []string

type Plugin[S proto.Message] struct {
	name            string
	runToExecArgs   RunToExecArgsFunc[S]
	parseConfig     func(ctx context.Context, ref *pluginv1.TargetRef, config map[string]*structpb.Value) (*pluginv1.TargetDef, error)
	getExecTarget   func(S) *execv1.Target
	applyTransitive func(context.Context, *pluginv1.TargetRef, *pluginv1.Sandbox, S) (*pluginv1.TargetDef, error)
	path            []string
	pathStr         string

	pipes  map[string]*pipe
	pipesm sync.RWMutex
}

func (p *Plugin[S]) ApplyTransitive(ctx context.Context, req *pluginv1.ApplyTransitiveRequest) (*pluginv1.ApplyTransitiveResponse, error) {
	ts := hproto.New[S]()
	err := req.GetTarget().GetDef().UnmarshalTo(ts)
	if err != nil {
		return nil, err
	}

	target, err := p.applyTransitive(ctx, req.GetTarget().GetRef(), req.GetTransitive(), ts)
	if err != nil {
		return nil, err
	}

	return pluginv1.ApplyTransitiveResponse_builder{
		Target: target,
	}.Build(), nil
}

func (p *Plugin[S]) PipesHandler() (string, http.Handler) {
	return PipesHandlerPath + "/", PipesHandler[S]{p}
}

func (p *Plugin[S]) Pipe(ctx context.Context, req *pluginv1.PipeRequest) (*pluginv1.PipeResponse, error) {
	p.pipesm.Lock()
	defer p.pipesm.Unlock()

	id := uuid.New().String()

	r, w := io.Pipe()

	p.pipes[id] = &pipe{exp: time.Now().Add(time.Minute), r: r, w: w}

	return pluginv1.PipeResponse_builder{
		Path: htypes.Ptr(path.Join(PipesHandlerPath, id)),
		Id:   htypes.Ptr(id),
	}.Build(), nil
}

func (p *Plugin[S]) Config(ctx context.Context, c *pluginv1.ConfigRequest) (*pluginv1.ConfigResponse, error) {
	desc := (&execv1.Target{}).ProtoReflect().Descriptor()
	pdesc := protodesc.ToDescriptorProto(desc)

	return pluginv1.ConfigResponse_builder{
		Name:         htypes.Ptr(p.name),
		TargetSchema: pdesc,
	}.Build(), nil
}

func (p *Plugin[S]) Parse(ctx context.Context, req *pluginv1.ParseRequest) (*pluginv1.ParseResponse, error) {
	target, err := p.parseConfig(ctx, req.GetSpec().GetRef(), req.GetSpec().GetConfig())
	if err != nil {
		return nil, err
	}

	return pluginv1.ParseResponse_builder{
		Target: target,
	}.Build(), nil
}

var _ pluginsdk.Driver = (*Plugin[*execv1.Target])(nil)
