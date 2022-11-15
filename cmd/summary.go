package cmd

import (
	"github.com/olekukonko/tablewriter"
	"heph/engine/htrace"
	"heph/utils"
	"os"
	"sort"
)

func summaryPhaseString(phases ...htrace.TargetStatsSpanPhase) string {
	opts := make([]summaryOpt, 0)
	for _, phase := range phases {
		opts = append(opts, summaryOpt{phase: phase})
	}
	return summaryPhaseStringOpt(opts...)
}

type summaryOpt struct {
	phase     htrace.TargetStatsSpanPhase
	decorator func(s string) string
}

func summaryPhaseStringOpt(phases ...summaryOpt) string {
	for _, opt := range phases {
		phase := opt.phase
		if phase.Name == "" {
			continue
		}

		s := utils.RoundDuration(phase.End.Sub(phase.Start), 1).String()

		if opt.decorator != nil {
			return opt.decorator(s)
		}

		return s
	}

	return ""
}

func PrintSummary(stats *htrace.Stats) {
	spans := make([]htrace.TargetStatsSpan, 0)
	for _, span := range stats.Spans {
		spans = append(spans, span)
	}

	sort.SliceStable(spans, func(i, j int) bool {
		return spans[i].Duration() > spans[j].Duration()
	})

	data := make([][]string, 0)
	for _, span := range spans {
		row := []string{
			span.FQN,
			summaryPhaseStringOpt(
				summaryOpt{
					phase: span.PhaseCachePull(),
					decorator: func(s string) string {
						if span.CacheHit {
							return s + " (hit)"
						}

						return s
					},
				},
				summaryOpt{
					phase: span.PhaseCachePullMeta(),
					decorator: func(s string) string {
						if span.CacheHit {
							s = s + " (hit)"
						}

						return s + " (meta)"
					},
				},
			),
			summaryPhaseString(span.PhaseCachePrepare()),
			summaryPhaseString(span.PhaseRunExec()),
			summaryPhaseString(span.PhaseRunCollectOutput()),
			summaryPhaseString(span.PhaseCacheStore()),
			utils.RoundDuration(span.Duration(), 1).String(),
		}

		data = append(data, row)
	}

	table := tablewriter.NewWriter(os.Stderr)
	table.SetAutoFormatHeaders(false)
	table.SetHeader([]string{"Target", "Cache Pull", "Prepare", "Exec", "Collect Output", "Cache Store", "Total"})
	table.SetBorder(true)
	table.AppendBulk(data)
	table.Render()
}
