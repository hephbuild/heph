package engine

import (
	"encoding/json"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/hash"
	"os"
	"path/filepath"
)

type AutocompleteCache struct {
	Hash          string
	AllTargets    []string
	Labels        []string
	PublicTargets []string
}

func (e *Engine) autocompleteCachePath() string {
	return filepath.Join(e.HomeDir.Abs(), "tmp", "autocomplete")
}

func (e *Engine) computeAutocompleteHash() (string, error) {
	h := hash.NewHash()
	h.I64(1)
	h.String(utils.Version)

	for _, file := range e.SourceFiles {
		h.String(file.Path)
		err := e.hashFilePath(h, file.Path)
		if err != nil {
			return "", err
		}
	}

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

func FilterPublicTargets(targets []*Target) []*Target {
	pubTargets := make([]*Target, 0)
	for _, target := range targets {
		if !target.IsPrivate() {
			pubTargets = append(pubTargets, target)
		}
	}
	return pubTargets
}

func (e *Engine) StoreAutocompleteCache() error {
	allTargets := make([]string, 0, e.Targets.Len())
	pubTargets := make([]string, 0, e.Targets.Len()/2)

	for _, fqn := range e.Targets.FQNs() {
		tp, _ := targetspec.TargetParse("", fqn)

		allTargets = append(allTargets, fqn)

		if !tp.IsPrivate() {
			pubTargets = append(pubTargets, fqn)
		}
	}

	cache := &AutocompleteCache{
		Hash:          e.autocompleteHash,
		AllTargets:    allTargets,
		PublicTargets: pubTargets,
		Labels:        e.Labels.Slice(),
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
