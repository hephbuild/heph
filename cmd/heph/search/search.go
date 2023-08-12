package search

import (
	"context"
	"fmt"
	"github.com/blevesearch/bleve/v2"
	"github.com/blevesearch/bleve/v2/analysis/analyzer/custom"
	"github.com/blevesearch/bleve/v2/analysis/token/lowercase"
	"github.com/blevesearch/bleve/v2/analysis/tokenizer/regexp"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/sets"
)

type Result struct {
	Targets specs.Targets
	Total   uint64
}

type Func func(querys string, max int) (Result, error)
type FuncCtx func(ctx context.Context, querys string, max int) (Result, error)

func Search(targets specs.Targets, query string) error {
	search, err := NewSearch(targets)
	if err != nil {
		return err
	}

	res, err := search(query, 10)
	if err != nil {
		return err
	}

	for _, target := range res.Targets {
		fmt.Println(target.Addr)
	}

	if res.Total > 10 {
		fmt.Printf("and %v more...\n", res.Total-10)
	}

	return nil
}

func NewSearch(targets specs.Targets) (Func, error) {
	search, err := NewSearchCtx(targets)
	if err != nil {
		return nil, err
	}

	return func(querys string, max int) (Result, error) {
		return search(context.Background(), querys, max)
	}, nil
}

func NewSearchCtx(targets specs.Targets) (FuncCtx, error) {
	ts := sets.NewSetFrom(func(t specs.Target) string {
		return t.Addr
	}, targets)

	mapping := bleve.NewIndexMapping()

	err := mapping.AddCustomTokenizer("techy", map[string]interface{}{
		"type":   regexp.Name,
		"regexp": `[0-9A-Za-z]+`,
	})
	if err != nil {
		return nil, err
	}

	// A custom analyzer for techy stuff
	err = mapping.AddCustomAnalyzer("techy", map[string]interface{}{
		"type":      custom.Name,
		"tokenizer": "techy",
		"token_filters": []string{
			lowercase.Name,
		},
	})
	if err != nil {
		return nil, err
	}
	mapping.DefaultAnalyzer = "techy"

	idx, err := bleve.NewMemOnly(mapping)
	if err != nil {
		return nil, err
	}

	specMapping := bleve.NewDocumentMapping()
	mapping.AddDocumentMapping("spec", specMapping)

	for _, name := range []string{"addr", "pkg", "name", "doc"} {
		simpleMapping := bleve.NewTextFieldMapping()
		simpleMapping.Analyzer = "techy"
		simpleMapping.Store = false
		specMapping.AddFieldMappingsAt(name, simpleMapping)
	}

	for _, target := range targets {
		err = idx.Index(target.Addr, struct {
			Name    string `json:"name"`
			Package string `json:"pkg"`
			Addr    string `json:"addr"`
			Doc     string `json:"doc"`
		}{
			Name:    target.Name,
			Package: target.Package.Path,
			Addr:    target.Addr,
			Doc:     target.Doc,
		})
		if err != nil {
			return nil, err
		}
	}

	return func(ctx context.Context, querys string, max int) (Result, error) {
		if querys == "" {
			return Result{}, nil
		}

		fzfq := newFzfQuery(querys, 2)

		qsq := bleve.NewQueryStringQuery(querys)

		q := bleve.NewDisjunctionQuery(
			fzfq,
			qsq,
		)

		sreq := bleve.NewSearchRequest(q)
		sreq.Size = max

		searchResults, err := idx.SearchInContext(ctx, sreq)
		if err != nil {
			return Result{}, err
		}

		targets := sets.NewSet(func(t specs.Target) string {
			return t.Addr
		}, searchResults.Hits.Len())
		for _, hit := range searchResults.Hits {
			targets.Add(ts.GetKey(hit.ID))
		}

		return Result{
			Targets: targets.Slice(),
			Total:   searchResults.Total,
		}, nil
	}, nil
}
