//go:build !nocloud

package hephcloud

import (
	"context"
	"encoding/json"
	"fmt"
	"github.com/Khan/genqlient/graphql"
	ajson "github.com/aarondl/json"
	"github.com/aarondl/opt/null"
	"github.com/hephbuild/heph/cloudclient"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/engine/observability"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/queue"
	"io"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
	"unsafe"
)

type Hook struct {
	observability.BaseHook
	Config    *config.Config
	Client    graphql.Client
	ProjectID string

	flowId       string
	invocationID string

	events queue.Queue[cloudclient.Event]
	logs   []*spanLogBuffer
	logsm  sync.Mutex
}

func NewHook(h *Hook) *Hook {
	h.events.Max = (200 * oneMegabyte) / int(unsafe.Sizeof(cloudclient.Event{}))
	return h
}

type spanLogBuffer struct {
	q           queue.Queue[byte]
	spanId      string
	localSpanId string
}

var _ io.Writer = (*spanLogBuffer)(nil)

func (l *spanLogBuffer) Write(p []byte) (n int, err error) {
	l.q.Enqueue(p...)
	return len(p), nil
}

func (h *Hook) Start(ctx context.Context) func() {
	stopCh := make(chan struct{})
	spansDoneCh := make(chan struct{})
	logsDoneCh := make(chan struct{})

	go func() {
		defer close(spansDoneCh)
		t := time.NewTicker(200 * time.Millisecond)
		defer t.Stop()

		for {
			select {
			case <-stopCh:
				return
			case <-t.C:
				err := h.sendSpans(ctx)
				if err != nil {
					log.Error("heph spans:", err)
				}
			}
		}
	}()

	go func() {
		defer close(logsDoneCh)
		t := time.NewTicker(200 * time.Millisecond)
		defer t.Stop()

		for {
			select {
			case <-stopCh:
				return
			case <-t.C:
				err := h.sendLogs(ctx)
				if err != nil {
					log.Error("heph logs:", err)
				}
			}
		}
	}()

	return func() {
		close(stopCh)
		h.events.DisableRescheduling = true
		h.logsm.Lock()
		for _, buf := range h.logs {
			buf.q.DisableRescheduling = true
		}
		h.logsm.Unlock()
		<-spansDoneCh
		<-logsDoneCh

		err := h.sendSpans(context.Background())
		if err != nil {
			log.Error("heph spans flush:", err)
		}

		err = h.sendLogs(context.Background())
		if err != nil {
			log.Error("heph logs flush:", err)
		}
	}
}

const oneMegabyte = 1000000

func (h *Hook) sendLogs(ctx context.Context) error {
	logs := h.logs[:]

	chunkSize := oneMegabyte

	for _, buf := range logs {
		if buf.spanId == "" {
			continue
		}

		err := buf.q.DequeueChunk(chunkSize, func(b []byte) error {
			_, err := cloudclient.SendLogs(ctx, h.Client, buf.spanId, string(b))

			return err
		})
		if err != nil {
			return err
		}
	}

	return nil
}

func (h *Hook) sendSpans(ctx context.Context) error {
	return h.events.DequeueChunk(1000, func(events []cloudclient.Event) error {
		gqlEvents := make([]json.RawMessage, 0, len(events))
		for _, event := range events {
			b, err := ajson.Marshal(event)
			if err != nil {
				return err
			}
			gqlEvents = append(gqlEvents, b)
		}

		log.Debugf("Sending %v events", len(gqlEvents))

		res, err := cloudclient.SendEvents(ctx, h.Client, h.invocationID, gqlEvents)
		if err != nil {
			return err
		}

		for _, span := range res.IngestSpans {
			for _, buf := range h.logs {
				if buf.localSpanId == span.SpanId {
					buf.spanId = span.Id
					break
				}
			}
		}

		return nil
	})
}

func (h *Hook) on(ctx context.Context, typ cloudclient.TargetExecSpanEvent, span span) (context.Context, observability.SpanHook) {
	cctx := h.newEvent(ctx, eventFactory(ctx, typ, span))

	return cctx, observability.AllSpanHook(func() {
		h.enqueueEvent(eventFactory(ctx, typ, span))
	})
}

type parentEventKey struct{}

func (h *Hook) newEvent(ctx context.Context, event cloudclient.Event) context.Context {
	h.enqueueEvent(event)

	ctx = context.WithValue(ctx, parentEventKey{}, event)

	return ctx
}

func (h *Hook) enqueueEvent(event cloudclient.Event) {
	if h.invocationID == "" {
		return
	}

	h.events.Enqueue(event)
}

type span interface {
	observability.SpanError
	Target() *tgt.Target
}

func nullFromTime(t time.Time) null.Val[time.Time] {
	if t.IsZero() {
		return null.FromPtr[time.Time](nil)
	}

	return null.From(t)
}

func eventFactory(ctx context.Context, eventType cloudclient.TargetExecSpanEvent, span span) cloudclient.Event {
	parentId := ""
	if parent, ok := ctx.Value(parentEventKey{}).(cloudclient.Event); ok {
		parentId = parent.SpanID
	}

	espan := cloudclient.Event{
		Event:         eventType,
		SpanID:        strconv.FormatUint(span.ID(), 10),
		ScheduledTime: nullFromTime(span.ScheduledTime()),
		QueuedTime:    nullFromTime(span.QueuedTime()),
		StartTime:     nullFromTime(span.StartTime()),
		EndTime:       nullFromTime(span.EndTime()),
		TargetFQN:     null.From(span.Target().FQN),
		ParentID:      null.From(parentId),
	}

	if !span.EndTime().IsZero() {
		switch span.FinalState() {
		case observability.StateSuccess:
			espan.FinalState = null.From(cloudclient.TargetExecSpanFinalStateSUCCESS)
		case observability.StateFailed:
			espan.FinalState = null.From(cloudclient.TargetExecSpanFinalStateFAILED)
		case observability.StateSkipped:
			espan.FinalState = null.From(cloudclient.TargetExecSpanFinalStateSKIPPED)
		}
	}

	switch span := span.(type) {
	case *observability.TargetExecSpan:
		// nothing special
	case *observability.ExternalCacheGetSpan:
		espan.CacheHit = null.FromPtr(span.IsCacheHit())
	case *observability.TargetArtifactCacheSpan:
		espan.ArtifactName = null.From(span.Artifact().Name())
		espan.CacheHit = null.FromPtr(span.IsCacheHit())
	}

	return espan
}

func (h *Hook) OnRoot(ctx context.Context, span *observability.BaseSpan) (context.Context, observability.SpanHook) {
	cfgb, err := json.Marshal(h.Config)
	if err != nil {
		log.Error(err)
		return ctx, nil
	}

	args := os.Args
	if len(args) > 0 {
		args[0] = "heph"
	}

	invInput := cloudclient.InvocationInput{
		Args:      args,
		Config:    cfgb,
		StartTime: span.StartTime(),
	}

	if flowId := strings.TrimSpace(os.Getenv("HEPH_FLOW_ID")); flowId == "" {
		name := strings.TrimSpace(os.Getenv("HEPH_FLOW_NAME"))
		if name == "" {
			name = strings.Join(args, " ")
		}

		res, err := cloudclient.RegisterFlowInvocation(ctx, h.Client, h.ProjectID, cloudclient.FlowInput{
			Name: name,
		}, invInput)
		if err != nil {
			log.Error(err)
			return ctx, nil
		}

		flow := res.RegisterFlowInvocation
		edges := flow.Invocations.Edges

		if len(edges) == 0 {
			return ctx, nil
		}

		h.invocationID = edges[0].Node.Id
		h.flowId = res.RegisterFlowInvocation.Id
	} else {
		res, err := cloudclient.RegisterInvocation(ctx, h.Client, flowId, invInput)
		if err != nil {
			log.Error(err)
			return ctx, nil
		}

		h.invocationID = res.RegisterInvocation.Id
		h.flowId = flowId
	}

	stopCh := make(chan struct{})

	go func() {
		for {
			select {
			case <-stopCh:
				return
			case <-time.After(10 * time.Second):
				_, err := cloudclient.SendInvocationHeartbeat(ctx, h.Client, h.invocationID)
				if err != nil {
					log.Error(err)
				}
			}
		}
	}()

	return ctx, observability.FinalizerSpanHook(func() {
		close(stopCh)

		_, err := cloudclient.SendEndInvocation(ctx, h.Client, cloudclient.InvocationEndInput{
			InvocationId: h.invocationID,
			Error:        span.Error() != nil,
		})
		if err != nil {
			log.Error(err)
		}
	})
}

func (h *Hook) OnRun(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventRUN, span)
}

func (h *Hook) OnCacheDownload(ctx context.Context, span *observability.TargetArtifactCacheSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventARTIFACT_REMOTE_DOWNLOAD, span)
}

func (h *Hook) OnCacheUpload(ctx context.Context, span *observability.TargetArtifactCacheSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventARTIFACT_REMOTE_UPLOAD, span)
}

func (h *Hook) OnRunPrepare(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventRUN_PREPARE, span)
}

type execLogsKey struct{}

func (h *Hook) OnRunExec(ctx context.Context, span *observability.TargetExecSpan) (context.Context, observability.SpanHook) {
	cctx := h.newEvent(ctx, eventFactory(ctx, cloudclient.TargetExecSpanEventRUN_EXEC, span))

	h.logsm.Lock()
	w := &spanLogBuffer{localSpanId: fmt.Sprint(span.ID())}
	w.q.Max = 200 * oneMegabyte
	h.logs = append(h.logs, w)
	h.logsm.Unlock()

	cctx = context.WithValue(cctx, execLogsKey{}, w)

	return cctx, observability.AllSpanHook(func() {
		h.enqueueEvent(eventFactory(ctx, cloudclient.TargetExecSpanEventRUN_EXEC, span))
	})
}

func (h *Hook) OnCollectOutput(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventCACHE_COLLECT_OUTPUT, span)
}

func (h *Hook) OnLocalCacheStore(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventCACHE_LOCAL_STORE, span)
}

func (h *Hook) OnLocalCacheCheck(ctx context.Context, span *observability.TargetArtifactCacheSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventCACHE_LOCAL_CHECK, span)
}

func (h *Hook) OnExternalCacheGet(ctx context.Context, span *observability.ExternalCacheGetSpan) (context.Context, observability.SpanHook) {
	return h.on(ctx, cloudclient.TargetExecSpanEventCACHE_REMOTE_GET, span)
}

func (h *Hook) OnLogs(ctx context.Context) io.Writer {
	w, ok := ctx.Value(execLogsKey{}).(*spanLogBuffer)
	if !ok {
		log.Debug("no lockBuffer found")
		return nil
	}

	return w
}

func (h *Hook) GetFlowID() string {
	return h.flowId
}
