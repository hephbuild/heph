package search

import (
	"fmt"
	"github.com/blevesearch/bleve/v2"
	"github.com/blevesearch/bleve/v2/analysis/analyzer/simple"
	lang_en "github.com/blevesearch/bleve/v2/analysis/lang/en"
	"heph/targetspec"
	"heph/utils/sets"
)

type Result struct {
	Targets targetspec.TargetSpecs
	Total   uint64
}

type Func func(querys string, max int) (Result, error)

func Search(targets targetspec.TargetSpecs, query string) error {
	search, err := NewSearch(targets)
	if err != nil {
		return err
	}

	res, err := search(query, 10)
	if err != nil {
		return err
	}

	for _, target := range res.Targets {
		fmt.Println(target.FQN)
	}

	if res.Total > 10 {
		fmt.Printf("and %v more...\n", res.Total-10)
	}

	return nil
}

func NewSearch(targets targetspec.TargetSpecs) (Func, error) {
	ts := sets.NewSetFrom(func(t targetspec.TargetSpec) string {
		return t.FQN
	}, targets)

	mapping := bleve.NewIndexMapping()

	idx, err := bleve.NewMemOnly(mapping)
	if err != nil {
		return nil, err
	}

	specMapping := bleve.NewDocumentMapping()
	mapping.AddDocumentMapping("spec", specMapping)

	for _, name := range []string{"fqn", "pkg", "name", "doc"} {
		simpleMapping := bleve.NewTextFieldMapping()
		simpleMapping.Store = false
		simpleMapping.Analyzer = simple.Name
		specMapping.AddFieldMappingsAt(name, simpleMapping)

		langMapping := bleve.NewTextFieldMapping()
		langMapping.Analyzer = lang_en.AnalyzerName
		langMapping.IncludeTermVectors = true
		langMapping.Store = false
		specMapping.AddFieldMappingsAt(name, langMapping)
	}

	for _, target := range targets {
		err = idx.Index(target.FQN, struct {
			Name    string `json:"name"`
			Package string `json:"pkg"`
			FQN     string `json:"fqn"`
			Doc     string `json:"doc"`
		}{
			Name:    target.Name,
			Package: target.Package.FullName,
			FQN:     target.FQN,
			Doc:     target.Doc,
		})
		if err != nil {
			return nil, err
		}
	}

	return func(querys string, max int) (Result, error) {
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

		searchResults, err := idx.Search(sreq)
		if err != nil {
			return Result{}, err
		}

		targets := sets.NewSet(func(t targetspec.TargetSpec) string {
			return t.FQN
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
