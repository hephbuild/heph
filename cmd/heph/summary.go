package main

import (
	"github.com/olekukonko/tablewriter"
	"heph/engine/htrace"
	"heph/utils"
	"heph/utils/sets"
	"os"
	"sort"
)

func summarySpanString(phases ...*htrace.TargetStatsSpan) string {
	opts := make([]summaryOpt, 0)
	for _, phase := range phases {
		opts = append(opts, summaryOpt{span: phase})
	}
	return summarySpanStringOpt(opts...)
}

type summaryOpt struct {
	span      *htrace.TargetStatsSpan
	decorator func(s string) string
}

func summarySpanStringOpt(phases ...summaryOpt) string {
	for _, opt := range phases {
		span := opt.span
		if span == nil {
			continue
		}

		s := utils.RoundDuration(span.End.Sub(span.Start), 1).String()

		if opt.decorator != nil {
			return opt.decorator(s)
		}

		return s
	}

	return ""
}

func artifactString(a htrace.TargetStatsArtifact, hitText string) string {
	if a.Name == "" {
		return ""
	}
	s := utils.RoundDuration(a.Duration(), 1).String()
	if hitText != "" && a.CacheHit {
		s = s + " (" + hitText + ")"
	}
	return s
}

func PrintSummary(stats *htrace.Stats, withGen bool) {
	targets := make([]*htrace.TargetStats, 0)
	for _, span := range stats.Spans {
		if !withGen && span.Gen {
			continue
		}

		targets = append(targets, span)
	}

	sort.Slice(targets, func(i, j int) bool {
		return targets[i].Duration() > targets[j].Duration()
	})

	data := make([][]string, 0)
	for _, target := range targets {
		row := []string{
			func() string {
				s := target.FQN
				if target.HasError() {
					s += " (error)"
				}

				return s
			}(),
			"",
			summarySpanString(target.Prepare),
			summarySpanString(target.Exec),
			summarySpanString(target.CollectOutput),
			summarySpanString(target.CacheStore),
			utils.RoundDuration(target.Duration(), 1).String(),
		}

		data = append(data, row)

		artifactsSet := sets.NewStringSet(0)
		for _, artifact := range target.ArtifactsUpload {
			artifactsSet.Add(artifact.Name)
		}
		for _, artifact := range target.ArtifactsDownload {
			artifactsSet.Add(artifact.Name)
		}
		for _, artifact := range target.ArtifactsLocalGet {
			artifactsSet.Add(artifact.Name)
		}
		artifacts := artifactsSet.Slice()
		sort.Strings(artifacts)

		for _, name := range artifacts {
			artifactLocalGet := target.ArtifactsLocalGet.Find(name)
			artifactPull := target.ArtifactsDownload.Find(name)
			artifactPush := target.ArtifactsUpload.Find(name)

			artifactGet := artifactPull
			if artifactGet.DisplayName == "" {
				artifactGet = artifactLocalGet
			}

			displayName := artifactGet.DisplayName

			if displayName == "" {
				displayName = artifactPush.DisplayName
			}

			data = append(data, []string{
				"  |" + displayName,
				func() string {
					if artifactLocalGet.Name == "" {
						if artifactPull.Name == "" {
							return ""
						}

						return artifactString(artifactPull, "hit remote")
					}

					return artifactString(artifactGet, "hit")
				}(),
				"",
				"",
				"",
				artifactString(artifactPush, ""),
				"",
			})
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
