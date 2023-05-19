package engine

import (
	"compress/gzip"
	"context"
	"errors"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/hash"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/vmihailenco/msgpack/v5"
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
	return filepath.Join(e.Root.Home.Abs(), "tmp", "autocomplete")
}

func (e *Engine) autocompleteCacheHashPath() string {
	return filepath.Join(e.Root.Home.Abs(), "tmp", "autocomplete_hash")
}

func (e *Engine) computeAutocompleteHash() (string, error) {
	h := hash.NewHash()
	h.I64(3)
	h.String(utils.Version)

	// TODO
	//for _, file := range e.SourceFiles {
	//	h.String(file.Path)
	//	err := e.hashFilePath(h, file.Path)
	//	if err != nil {
	//		return "", err
	//	}
	//}
	//
	//fqns := e.Targets.FQNs()
	//sort.Strings(fqns)
	//for _, fqn := range fqns {
	//	h.String(fqn)
	//}
	//
	//for _, target := range e.linkedTargets().Slice() {
	//	if !target.Gen {
	//		continue
	//	}
	//
	//	h.String(target.FQN)
	//
	//	err := e.hashInputFiles(h, target)
	//	if err != nil {
	//		return "", err
	//	}
	//}

	return h.Sum(), nil
}

func (e *Engine) StoreAutocompleteCache(ctx context.Context) error {
	err := e.autocompleteCacheLock.Lock(ctx)
	if err != nil {
		return err
	}
	defer e.autocompleteCacheLock.Unlock()

	prevHash, _ := e.LoadAutocompleteCacheHash()

	if prevHash == e.autocompleteHash {
		return nil
	}

	allTargets := make([]targetspec.TargetSpec, 0, e.Graph.Targets().Len())
	for _, target := range e.Graph.Targets().Slice() {
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

	doneCloser := utils.CloserContext(f, ctx)
	defer doneCloser()

	gw := gzip.NewWriter(f)
	defer gw.Close()

	enc := msgpack.NewEncoder(gw)
	err = enc.Encode(cache)
	if err != nil {
		return err
	}

	err = os.WriteFile(e.autocompleteCacheHashPath(), []byte(e.autocompleteHash), os.ModePerm)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) LoadAutocompleteCacheHash() (string, error) {
	// TODO
	return "", nil

	b, err := os.ReadFile(e.autocompleteCacheHashPath())
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return "", err
	}

	return string(b), nil
}

func (e *Engine) LoadAutocompleteCache() (*AutocompleteCache, error) {
	if e.autocompleteHash == "" {
		return nil, nil
	}

	hashFromFile, _ := e.LoadAutocompleteCacheHash()

	if e.autocompleteHash != hashFromFile {
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
