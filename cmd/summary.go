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
		inlineCachePull := len(span.CachePulls) == 1 && span.CachePulls[0].Name == ""

		cachePullCell := ""
		if inlineCachePull {
			cachePullCell = summaryPhaseStringOpt(
				summaryOpt{
					phase: span.PhaseCachePull(),
					decorator: func(s string) string {
						if span.CacheHit() {
							return s + " (hit)"
						}

						return s
					},
				},
				summaryOpt{
					phase: span.PhaseCachePullMeta(),
					decorator: func(s string) string {
						if span.CacheHit() {
							s = s + " (hit)"
						}

						return s + " (meta)"
					},
				},
			)
		}

		row := []string{
			func() string {
				s := span.FQN
				if span.Error {
					s += " (error)"
				}

				return s
			}(),
			cachePullCell,
			summaryPhaseString(span.PhaseCachePrepare()),
			summaryPhaseString(span.PhaseRunExec()),
			summaryPhaseString(span.PhaseRunCollectOutput()),
			summaryPhaseString(span.PhaseCacheStore()),
			utils.RoundDuration(span.Duration(), 1).String(),
		}

		data = append(data, row)

		if !inlineCachePull {
			sort.SliceStable(span.CachePulls, func(i, j int) bool {
				return span.CachePulls[i].Name < span.CachePulls[j].Name
			})

			for _, pull := range span.CachePulls {
				name := pull.Name
				if name == "" {
					name = "<unnamed>"
				}
				data = append(data, []string{
					"  |" + name,
					func() string {
						s := utils.RoundDuration(pull.Duration(), 1).String()
						if span.CacheHit() {
							s = s + " (hit)"
						}
						return s
					}(),
					"",
					"",
					"",
					"",
					"",
				})
			}
		}
	}

	table := tablewriter.NewWriter(os.Stderr)
	table.SetAutoFormatHeaders(false)
	table.SetAutoWrapText(false)
	table.SetHeader([]string{"Target", "Cache Pull", "Prepare", "Exec", "Collect Output", "Cache Store", "Total"})
	if stats.RootSpan != nil {
		table.SetFooter([]string{"Total", "", "", "", "", "", utils.RoundDuration(stats.RootSpan.EndTime().Sub(stats.RootSpan.StartTime()), 1).String()})
	}
	table.SetBorder(true)
	table.AppendBulk(data)
	table.Render()
}
