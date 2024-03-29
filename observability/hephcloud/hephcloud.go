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
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/utils/ads"
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
	FlowId    string

	invocationID string

	events queue.Queue[cloudclient.Event]
	logs   []*spanLogBuffer
	logsm  sync.Mutex
}

func NewHook(h *Hook) *Hook {
	// queue 200 MB worth of events
	h.events.Max = (200 * oneMegabyteInBytes) / int(unsafe.Sizeof(cloudclient.Event{}))
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
	spansDoneCh := make(chan struct{})
	logsDoneCh := make(chan struct{})

	terminate := false

	ctx, cancel := context.WithCancelCause(context.Background())

	go func() {
		defer close(spansDoneCh)
		t := time.NewTicker(200 * time.Millisecond)
		defer t.Stop()

		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				if terminate && h.events.Len() == 0 {
					return
				}
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
			case <-ctx.Done():
				return
			case <-t.C:
				if terminate && h.logsAllEmpty() {
					return
				}
				err := h.sendLogs(ctx)
				if err != nil {
					log.Error("heph logs:", err)
				}
			}
		}
	}()

	return func() {
		terminate = true
		defer cancel(context.Canceled)
		t := time.AfterFunc(30*time.Second, func() {
			cancel(context.DeadlineExceeded)
		})
		defer t.Stop()

		go func() {
			select {
			case <-ctx.Done():
			case <-time.After(time.Second):
				log.Info("Sending remaining telemetry...")
			}
		}()

		<-spansDoneCh
		<-logsDoneCh

		if l := h.events.Len(); l > 0 {
			log.Warnf("Unsent spans: %v", l)
		}

		unsentBytes := uint64(0)
		for _, buf := range h.logsCopy() {
			unsentBytes += uint64(buf.q.Len())
		}
		if unsentBytes > 0 {
			log.Warnf("Unsent log bytes: %v", unsentBytes)
		}
	}
}

const oneMegabyteInBytes = 1000000

func (h *Hook) logsCopy() []*spanLogBuffer {
	h.logsm.Lock()
	defer h.logsm.Unlock()

	return ads.Copy(h.logs)
}

func (h *Hook) logsAllEmpty() bool {
	h.logsm.Lock()
	defer h.logsm.Unlock()

	for _, buf := range h.logs {
		if buf.q.Len() != 0 {
			return false
		}
	}

	return true
}

func (h *Hook) sendLogs(ctx context.Context) error {
	chunkSize := oneMegabyteInBytes

	for _, buf := range h.logsCopy() {
		if buf.spanId == "" {
			continue
		}

		err := buf.q.DequeueChunkContext(ctx, chunkSize, func(b []byte) error {
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
	return h.events.DequeueChunkContext(ctx, 1000, func(events []cloudclient.Event) error {
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

		h.logsm.Lock()
		defer h.logsm.Unlock()

		for _, buf := range h.logs {
			for _, span := range res.IngestSpans {
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
	Target() *graph.Target
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
		TargetAddr:    null.From(span.Target().Addr),
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

	if flowId := h.FlowId; flowId == "" {
		res, err := cloudclient.RegisterFlowInvocation(ctx, h.Client, h.ProjectID, cloudclient.FlowInput{
			Name: strings.Join(args, " "),
		}, invInput)
		if err != nil {
			log.Error(fmt.Errorf("RegisterFlowInvocation: %v", err))
			return ctx, nil
		}

		flow := res.RegisterFlowInvocation
		edges := flow.Invocations.Edges

		if len(edges) == 0 {
			return ctx, nil
		}

		h.invocationID = edges[0].Node.Id
		h.FlowId = res.RegisterFlowInvocation.Id
	} else {
		res, err := cloudclient.RegisterInvocation(ctx, h.Client, flowId, invInput)
		if err != nil {
			log.Error(fmt.Errorf("RegisterInvocation: %v", err))
			return ctx, nil
		}

		h.invocationID = res.RegisterInvocation.Id
		h.FlowId = flowId
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
					log.Error(fmt.Errorf("heartbeat: %v", err))
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
			log.Error(fmt.Errorf("end invocation: %v", err))
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
	w.q.Max = 200 * oneMegabyteInBytes
	h.logs = append(h.logs, w)
	h.logsm.Unlock()

	cctx = context.WithValue(cctx, execLogsKey{}, w)

	return cctx, observability.AllSpanHook(func() {
		if span.FinalState() != observability.StateUnknown {
			go func() {
				<-w.q.Empty()
				h.logsm.Lock()
				h.logs = ads.Remove(h.logs, w)
				h.logsm.Unlock()
			}()
		}

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
	return h.FlowId
}
