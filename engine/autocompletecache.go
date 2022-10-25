package engine

import (
	"encoding/json"
	"heph/targetspec"
	"heph/utils"
	"os"
	"path/filepath"
)

type AutocompleteCache struct {
	Hash    string
	Targets []string
	Labels  []string
}

func (e *Engine) autocompleteCachePath() string {
	return filepath.Join(e.HomeDir.Abs(), "tmp", "autocomplete")
}

func (e *Engine) computeAutocompleteHash() (string, error) {
	h := utils.NewHash()
	h.String(utils.Version)

	for _, target := range e.linkedTargets().Slice() {
		if !target.Gen {
			continue
		}

		h.String(target.FQN)

		err := e.hashInputFiles(h, target)
		if err != nil {
			return "", err
		}
	}

	return h.Sum(), nil
}

func (e *Engine) StoreAutocompleteCache() error {
	targets := make([]string, 0, len(e.Targets.FQNs()))
	for _, fqn := range e.Targets.FQNs() {
		tp, _ := targetspec.TargetParse("", fqn)

		if tp.IsPrivate() {
			continue
		}

		targets = append(targets, fqn)
	}

	cache := &AutocompleteCache{
		Hash:    e.autocompleteHash,
		Targets: targets,
		Labels:  e.Labels,
	}

	b, err := json.Marshal(cache)
	if err != nil {
		return err
	}

	err = os.WriteFile(e.autocompleteCachePath(), b, os.ModePerm)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) LoadAutocompleteCache() (*AutocompleteCache, error) {
	if e.autocompleteHash == "" {
		return nil, nil
	}

	b, err := os.ReadFile(e.autocompleteCachePath())
	if err != nil {
		return nil, err
	}

	var cache AutocompleteCache
	err = json.Unmarshal(b, &cache)
	if err != nil {
		return nil, err
	}

	if cache.Hash == "" || cache.Hash != e.autocompleteHash {
		return nil, nil
	}

	return &cache, nil
}
