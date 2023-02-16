package engine

import (
	"compress/gzip"
	"errors"
	"github.com/vmihailenco/msgpack/v5"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/hash"
	"heph/utils/sets"
	"os"
	"path/filepath"
)

type AutocompleteCache struct {
	Hash    string
	Targets []targetspec.TargetSpec

	publicTargets utils.Once[[]targetspec.TargetSpec]
	labels        utils.Once[[]string]
}

func (a *AutocompleteCache) PublicTargets() []targetspec.TargetSpec {
	return a.publicTargets.MustDo(func() ([]targetspec.TargetSpec, error) {
		pub := make([]targetspec.TargetSpec, 0)
		for _, target := range a.Targets {
			if target.IsPrivate() {
				continue
			}

			pub = append(pub, target)
		}

		return pub, nil
	})
}

func (a *AutocompleteCache) Labels() []string {
	return a.labels.MustDo(func() ([]string, error) {
		labels := sets.NewStringSet(0)
		for _, target := range a.Targets {
			labels.AddAll(target.Labels)
		}

		return labels.Slice(), nil
	})
}

func (e *Engine) autocompleteCachePath() string {
	return filepath.Join(e.HomeDir.Abs(), "tmp", "autocomplete")
}

func (e *Engine) computeAutocompleteHash() (string, error) {
	h := hash.NewHash()
	h.I64(2)
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
	allTargets := make([]targetspec.TargetSpec, 0, e.Targets.Len())
	for _, target := range e.Targets.Slice() {
		allTargets = append(allTargets, target.TargetSpec)
	}

	cache := &AutocompleteCache{
		Hash:    e.autocompleteHash,
		Targets: allTargets,
	}

	f, err := os.Create(e.autocompleteCachePath())
	if err != nil {
		return err
	}
	defer f.Close()

	gw := gzip.NewWriter(f)
	defer gw.Close()

	enc := msgpack.NewEncoder(gw)
	err = enc.Encode(cache)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) LoadAutocompleteCache() (*AutocompleteCache, error) {
	if e.autocompleteHash == "" {
		return nil, nil
	}

	f, err := os.Open(e.autocompleteCachePath())
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, nil
		}
		return nil, err
	}
	defer f.Close()

	gr, err := gzip.NewReader(f)
	if err != nil {
		return nil, err
	}
	defer gr.Close()

	var cache AutocompleteCache
	dec := msgpack.NewDecoder(gr)
	err = dec.Decode(&cache)
	if err != nil {
		return nil, err
	}

	if cache.Hash == "" || cache.Hash != e.autocompleteHash {
		return nil, nil
	}

	return &cache, nil
}
