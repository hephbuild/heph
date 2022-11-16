package htrace

import (
	"context"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	tracesdk "go.opentelemetry.io/otel/sdk/trace"
	"heph/utils"
	"sync"
	"time"
)

const (
	PhaseTargetRun              = "target_run"
	PhaseGenPass                = "gen_pass"
	PhaseRunPrepare             = "run_prepare"
	PhaseRunExec                = "run_exec"
	PhaseRunCollectOutput       = "run_collect_output"
	PhaseRunCacheStore          = "run_cache_store"
	PhaseCachePull              = "cache_pull"
	PhaseCachePullMeta          = "cache_pull_meta"
	PhaseScheduleTargetWithDeps = "schedule_target_with_deps"
)

const (
	AttrPhase      = "heph.phase"
	AttrTargetAddr = "heph.target_addr"
	AttrCacheHit   = "heph.cache_hit"
)

var AllPhases = []string{
	PhaseTargetRun,
	PhaseRunPrepare,
	PhaseRunExec,
	PhaseRunCacheStore,
	PhaseRunCollectOutput,
	PhaseCachePull,
	PhaseCachePullMeta,
}

var TargetRunPhases = []string{
	PhaseCachePullMeta,
	PhaseCachePull,
	PhaseRunPrepare,
	PhaseRunExec,
	PhaseRunCollectOutput,
	PhaseRunCacheStore,
}

type Stats struct {
	Spans        map[string]TargetStatsSpan
	targetPhases map[string]map[string]TargetStatsSpanPhase
	spansm       sync.Mutex
}

var _ tracesdk.SpanProcessor = (*Stats)(nil)

var AttributeNotFound = attribute.Value{}

func findAttr(key string, attributes []attribute.KeyValue) attribute.Value {
	for _, a := range attributes {
		if string(a.Key) == key {
			return a.Value
		}
	}

	return AttributeNotFound
}

func (st *Stats) OnStart(parent context.Context, s tracesdk.ReadWriteSpan) {}

func (st *Stats) OnEnd(s tracesdk.ReadOnlySpan) {
	phase := findAttr(AttrPhase, s.Attributes()).AsString()

	if !utils.Contains(AllPhases, phase) {
		return
	}

	fqn := findAttr(AttrTargetAddr, s.Attributes()).AsString()

	if fqn == "" || phase == "" {
		return
	}

	st.spansm.Lock()
	defer st.spansm.Unlock()

	if utils.Contains(TargetRunPhases, phase) {
		if st.targetPhases == nil {
			st.targetPhases = map[string]map[string]TargetStatsSpanPhase{}
		}

		if st.targetPhases[fqn] == nil {
			st.targetPhases[fqn] = map[string]TargetStatsSpanPhase{}
		}

		st.targetPhases[fqn][phase] = TargetStatsSpanPhase{
			Name:  phase,
			Start: s.StartTime(),
			End:   s.EndTime(),
		}
	}

	if phase == PhaseTargetRun || phase == PhaseCachePull {
		if _, ok := st.targetPhases[fqn]; !ok {
			return
		}

		phases := make([]TargetStatsSpanPhase, 0)
		for _, phaseName := range TargetRunPhases {
			phase, ok := st.targetPhases[fqn][phaseName]
			if !ok {
				continue
			}

			phases = append(phases, phase)
		}

		cacheHit := false
		if v := findAttr(AttrCacheHit, s.Attributes()); v != AttributeNotFound {
			cacheHit = v.AsBool()
		}

		delete(st.targetPhases, fqn)

		if st.Spans == nil {
			st.Spans = map[string]TargetStatsSpan{}
		}

		stat, ok := st.Spans[fqn]
		if !ok {
			stat = TargetStatsSpan{
				FQN:   fqn,
				Start: s.StartTime(),
				End:   s.EndTime(),
			}
		}

		stat.End = s.EndTime()
		stat.Phases = append(stat.Phases, phases...)
		if cacheHit {
			stat.CacheHit = true
		}
		if s.Status().Code == codes.Error {
			stat.Error = true
		}
		st.Spans[fqn] = stat
	}
}

func (st *Stats) Shutdown(ctx context.Context) error {
	return nil
}

func (st *Stats) ForceFlush(ctx context.Context) error {
	return nil
}

func (st *Stats) Reset() {
	st.spansm.Lock()
	defer st.spansm.Unlock()

	st.Spans = map[string]TargetStatsSpan{}
	st.targetPhases = map[string]map[string]TargetStatsSpanPhase{}
}

type TargetStatsSpanPhase struct {
	Name  string
	Start time.Time
	End   time.Time
}

type TargetStatsSpan struct {
	FQN      string
	Start    time.Time
	End      time.Time
	Phases   []TargetStatsSpanPhase
	CacheHit bool
	Error    bool
}

func (s TargetStatsSpan) Duration() time.Duration {
	return s.End.Sub(s.Start)
}

func (s TargetStatsSpan) getPhase(name string) TargetStatsSpanPhase {
	for _, phase := range s.Phases {
		if phase.Name == name {
			return phase
		}
	}

	return TargetStatsSpanPhase{}
}

func (s TargetStatsSpan) PhaseCachePull() TargetStatsSpanPhase {
	return s.getPhase(PhaseCachePull)
}

func (s TargetStatsSpan) PhaseCachePullMeta() TargetStatsSpanPhase {
	return s.getPhase(PhaseCachePullMeta)
}

func (s TargetStatsSpan) PhaseCachePrepare() TargetStatsSpanPhase {
	return s.getPhase(PhaseRunPrepare)
}

func (s TargetStatsSpan) PhaseRunExec() TargetStatsSpanPhase {
	return s.getPhase(PhaseRunExec)
}

func (s TargetStatsSpan) PhaseRunCollectOutput() TargetStatsSpanPhase {
	return s.getPhase(PhaseRunCollectOutput)
}

func (s TargetStatsSpan) PhaseCacheStore() TargetStatsSpanPhase {
	return s.getPhase(PhaseRunCacheStore)
}
