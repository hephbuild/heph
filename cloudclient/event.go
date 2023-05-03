package cloudclient

import (
	"github.com/aarondl/opt/null"
	"time"
)

type TargetExecSpanEvent string

// Enum values for TargetExecSpanEvent
const (
	TargetExecSpanEventRUN                      TargetExecSpanEvent = "RUN"
	TargetExecSpanEventRUN_PREPARE              TargetExecSpanEvent = "RUN_PREPARE"
	TargetExecSpanEventRUN_EXEC                 TargetExecSpanEvent = "RUN_EXEC"
	TargetExecSpanEventGEN_PASS                 TargetExecSpanEvent = "GEN_PASS"
	TargetExecSpanEventCACHE_COLLECT_OUTPUT     TargetExecSpanEvent = "CACHE_COLLECT_OUTPUT"
	TargetExecSpanEventCACHE_LOCAL_STORE        TargetExecSpanEvent = "CACHE_LOCAL_STORE"
	TargetExecSpanEventCACHE_LOCAL_CHECK        TargetExecSpanEvent = "CACHE_LOCAL_CHECK"
	TargetExecSpanEventCACHE_REMOTE_GET         TargetExecSpanEvent = "CACHE_REMOTE_GET"
	TargetExecSpanEventARTIFACT_REMOTE_DOWNLOAD TargetExecSpanEvent = "ARTIFACT_REMOTE_DOWNLOAD"
	TargetExecSpanEventARTIFACT_REMOTE_UPLOAD   TargetExecSpanEvent = "ARTIFACT_REMOTE_UPLOAD"
)

type TargetExecSpanFinalState string

// Enum values for TargetExecSpanFinalState
const (
	TargetExecSpanFinalStateSUCCESS TargetExecSpanFinalState = "SUCCESS"
	TargetExecSpanFinalStateFAILED  TargetExecSpanFinalState = "FAILED"
	TargetExecSpanFinalStateSKIPPED TargetExecSpanFinalState = "SKIPPED"
)

type Event struct {
	Event         TargetExecSpanEvent                `json:"event"`
	SpanID        string                             `json:"span_id"`
	FinalState    null.Val[TargetExecSpanFinalState] `json:"final_state"`
	ScheduledTime null.Val[time.Time]                `json:"scheduled_time,omitempty"`
	QueuedTime    null.Val[time.Time]                `json:"queued_time,omitempty"`
	StartTime     null.Val[time.Time]                `json:"start_time,omitempty"`
	EndTime       null.Val[time.Time]                `json:"end_time,omitempty"`
	TargetFQN     null.Val[string]                   `json:"target_fqn,omitempty"`
	ArtifactName  null.Val[string]                   `json:"artifact_name,omitempty"`
	CacheHit      null.Val[bool]                     `json:"cache_hit,omitempty"`
	ParentID      null.Val[string]                   `json:"parent_id,omitempty"`
}
