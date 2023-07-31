package summary

import "time"

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
	Addr              string
	Prepare           *TargetStatsSpan
	Exec              *TargetStatsSpan
	CollectOutput     *TargetStatsSpan
	CacheStore        *TargetStatsSpan
	ArtifactsLocalGet TargetStatsArtifacts
	ArtifactsDownload TargetStatsArtifacts
	ArtifactsUpload   TargetStatsArtifacts
	Gen               bool

	duration time.Duration
}

func (s *TargetStats) Duration() time.Duration {
	if s.duration > 0 {
		return s.duration
	}

	var min, max time.Time
	for _, span := range []*TargetStatsSpan{s.Prepare, s.Exec, s.CollectOutput, s.CacheStore} {
		if span == nil {
			continue
		}

		if min.IsZero() || span.Start.Unix() < min.Unix() {
			min = span.Start
		}

		if max.IsZero() || span.End.Unix() > max.Unix() {
			max = span.End
		}
	}
	if !min.IsZero() {
		d := max.Sub(min)
		s.duration = d
		return d
	}

	var d time.Duration
	for _, as := range []TargetStatsArtifacts{s.ArtifactsLocalGet, s.ArtifactsDownload, s.ArtifactsUpload} {
		for _, span := range as {
			d += span.End.Sub(span.Start)
		}
	}

	s.duration = d
	return d
}

func (s *TargetStats) HasError() bool {
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
