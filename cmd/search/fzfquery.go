package search

import (
	"context"
	"github.com/blevesearch/bleve/v2/mapping"
	"github.com/blevesearch/bleve/v2/search"
	"github.com/blevesearch/bleve/v2/search/query"
	"github.com/blevesearch/bleve/v2/search/searcher"
	index "github.com/blevesearch/bleve_index_api"
	"github.com/lithammer/fuzzysearch/fuzzy"
)

type fzfQuery struct {
	s string
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

		score := fuzzy.RankMatchNormalizedFold(q.s, fqn)
		if score < 0 {
			return false
		}

		d.Score = float64(score)

		return true
	}), nil
}

func newFzfQuery(s string) query.Query {
	return fzfQuery{s: s}
}
