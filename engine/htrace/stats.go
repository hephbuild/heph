package htrace

import (
	"context"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	tracesdk "go.opentelemetry.io/otel/sdk/trace"
	log "heph/hlog"
	"heph/utils"
	"sync"
	"time"
)

const (
	TypeTargetRun              = "target_run"
	TypeGenPass                = "gen_pass"
	TypeRunPrepare             = "run_prepare"
	TypeRunExec                = "run_exec"
	TypeCollectOutput          = "collect_output"
	TypeLocalCacheStore        = "cache_store"
	TypeLocalCacheGet          = "cache_local_get"
	TypeExternalCacheGet       = "cache_external_get"
	TypeCacheDownload          = "cache_download"
	TypeCacheUpload            = "cache_upload"
	TypeScheduleTargetWithDeps = "schedule_target_with_deps"
)

const (
	AttrType                = "heph.type"
	AttrDuringGen           = "heph.during_gen"
	AttrRoot                = "heph.root"
	AttrArtifactName        = "heph.artifact_name"
	AttrArtifactDisplayName = "heph.artifact_display_name"
	AttrTargetAddr          = "heph.target_addr"
	AttrCacheHit            = "heph.cache_hit"
	AttrAfterPulling        = "heph.after_pulling"
)

var AllTypes = []string{
	TypeGenPass,
	TypeTargetRun,
	TypeRunPrepare,
	TypeRunExec,
	TypeCollectOutput,
	TypeLocalCacheStore,
	TypeLocalCacheGet,
	TypeExternalCacheGet,
	TypeCacheDownload,
	TypeCacheUpload,
	TypeScheduleTargetWithDeps,
}

type Stats struct {
	Spans    map[string]*TargetStats
	RootSpan tracesdk.ReadOnlySpan
	spansm   sync.Mutex
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

func (st *Stats) OnStart(parent context.Context, s tracesdk.ReadWriteSpan) {
	duringGen, _ := parent.Value(AttrDuringGen).(bool)
	if duringGen {
		s.SetAttributes(attribute.Bool(AttrDuringGen, true))
	}
}

func (st *Stats) OnEnd(s tracesdk.ReadOnlySpan) {
	if findAttr(AttrRoot, s.Attributes()).AsBool() {
		st.RootSpan = s
		return
	}

	typ := findAttr(AttrType, s.Attributes()).AsString()

	if !utils.Contains(AllTypes, typ) {
		return
	}

	fqn := findAttr(AttrTargetAddr, s.Attributes()).AsString()

	if fqn == "" || typ == "" {
		return
	}

	st.spansm.Lock()
	defer st.spansm.Unlock()

	if st.Spans == nil {
		st.Spans = map[string]*TargetStats{}
	}

	stat, ok := st.Spans[fqn]
	if !ok {
		stat = &TargetStats{
			FQN:   fqn,
			Start: s.StartTime(),
			End:   s.EndTime(),
		}
		st.Spans[fqn] = stat
	}

	if s.EndTime().Unix() > stat.End.Unix() {
		stat.End = s.EndTime()
	}

	tstat := TargetStatsSpan{}
	tstat.Start = s.StartTime()
	tstat.End = s.EndTime()
	if s.Status().Code == codes.Error {
		tstat.Error = true
	}
	if v := findAttr(AttrDuringGen, s.Attributes()); v != AttributeNotFound {
		stat.Gen = v.AsBool()
	}

	switch typ {
	case TypeRunPrepare:
		stat.Prepare = &tstat
	case TypeRunExec:
		stat.Exec = &tstat
	case TypeCollectOutput:
		stat.CollectOutput = &tstat
	case TypeLocalCacheStore:
		stat.CacheStore = &tstat
	case TypeCacheDownload, TypeCacheUpload, TypeLocalCacheGet:
		name := findAttr(AttrArtifactName, s.Attributes())
		displayName := findAttr(AttrArtifactDisplayName, s.Attributes())

		if name == AttributeNotFound || displayName == AttributeNotFound {
			log.Error("%v %v span is missing attribute %#v", fqn, typ, s.Attributes())
		}

		if typ == TypeLocalCacheGet {
			if v := findAttr(AttrAfterPulling, s.Attributes()); v != AttributeNotFound {
				return
			}
		}

		cacheHit := false
		if v := findAttr(AttrCacheHit, s.Attributes()); v != AttributeNotFound {
			cacheHit = v.AsBool()
		}

		astats := TargetStatsArtifact{
			Name:        name.AsString(),
			DisplayName: displayName.AsString(),
			Start:       tstat.Start,
			End:         tstat.End,
			CacheHit:    cacheHit,
		}
		if s.Status().Code == codes.Error {
			astats.Error = true
		}

		switch typ {
		case TypeCacheDownload:
			stat.ArtifactsDownload = append(stat.ArtifactsDownload, astats)
		case TypeCacheUpload:
			stat.ArtifactsUpload = append(stat.ArtifactsUpload, astats)
		case TypeLocalCacheGet:
			stat.ArtifactsLocalGet = append(stat.ArtifactsLocalGet, astats)
		}
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

	st.Spans = map[string]*TargetStats{}
}

type TargetStatsSpan struct {
	Start time.Time
	End   time.Time
	Error bool
}

type TargetStatsArtifact struct {
	Name        string
	DisplayName string
	Start       time.Time
	End         time.Time
	CacheHit    bool
	Error       bool
}

func (p TargetStatsArtifact) Duration() time.Duration {
	return p.End.Sub(p.Start)
}

type TargetStatsArtifacts []TargetStatsArtifact

func (as TargetStatsArtifacts) Find(name string) TargetStatsArtifact {
	for _, artifact := range as {
		if artifact.Name == name {
			return artifact
		}
	}

	return TargetStatsArtifact{}
}

type TargetStats struct {
	FQN               string
	Start             time.Time
	End               time.Time
	Prepare           *TargetStatsSpan
	Exec              *TargetStatsSpan
	CollectOutput     *TargetStatsSpan
	CacheStore        *TargetStatsSpan
	ArtifactsLocalGet TargetStatsArtifacts
	ArtifactsDownload TargetStatsArtifacts
	ArtifactsUpload   TargetStatsArtifacts
	Gen               bool
}

func (s TargetStats) Duration() time.Duration {
	return s.End.Sub(s.Start)
}

func (s TargetStats) HasError() bool {
	for _, span := range []*TargetStatsSpan{s.Prepare, s.Exec, s.CollectOutput, s.CacheStore} {
		if span == nil {
			continue
		}

		if span.Error {
			return true
		}
	}

	for _, as := range []TargetStatsArtifacts{s.ArtifactsLocalGet, s.ArtifactsDownload, s.ArtifactsUpload} {
		for _, a := range as {
			if a.Error {
				return true
			}
		}
	}

	return false
}
