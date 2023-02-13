package search

import (
	"context"
	"github.com/blevesearch/bleve/v2/mapping"
	"github.com/blevesearch/bleve/v2/search"
	"github.com/blevesearch/bleve/v2/search/query"
	"github.com/blevesearch/bleve/v2/search/searcher"
	index "github.com/blevesearch/bleve_index_api"
	"github.com/lithammer/fuzzysearch/fuzzy"
	"math"
)

type fzfQuery struct {
	s     string
	boost float64
}

func (q fzfQuery) Searcher(ctx context.Context, i index.IndexReader, m mapping.IndexMapping, options search.SearcherOptions) (search.Searcher, error) {
	qBase := query.NewMatchAllQuery()

	baseSearcher, err := qBase.Searcher(ctx, i, m, options)
	if err != nil {
		return nil, err
	}

	return searcher.NewFilteringSearcher(ctx, baseSearcher, func(d *search.DocumentMatch) bool {
		doc, err := i.Document(string(d.IndexInternalID))
		if err != nil || doc == nil {
			return false
		}

		fqn := ""
		doc.VisitFields(func(field index.Field) {
			if field.Name() == "fqn" {
				fqn = string(field.Value())
			}
		})
		if fqn == "" {
			return false
		}

		ld := fuzzy.RankMatchNormalizedFold(q.s, fqn)
		if ld < 0 {
			return false
		}

		score := math.Min(1, math.Pow(2, -float64(ld)/float64(len(fqn))))
		d.Score = (d.Score + score*q.boost) / (q.boost + 1)

		return true
	}), nil
}

func newFzfQuery(s string, boost float64) query.Query {
	return fzfQuery{s: s, boost: boost}
}
