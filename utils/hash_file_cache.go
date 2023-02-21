package utils

import (
	"encoding/json"
	"errors"
	log "heph/hlog"
	"os"
)

type HashCacheData[T any] struct {
	Hash string
	Data T
}

func HashCache[T any](path, h string, do func() (T, error)) (T, error) {
	var data HashCacheData[T]

	b, err := os.ReadFile(path)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		log.Warnf("cache: %v", err)
	}

	log.Tracef("hash: %v", h)

	if len(b) > 0 {
		err = json.Unmarshal(b, &data)
		if err != nil {
			log.Warnf("cache: %v", err)
		}
	}

	log.Tracef("cache hash: %v", data.Hash)

	if data.Hash == h {
		log.Tracef("cache hash match, serving %v", data.Data)
		return data.Data, nil
	}

	v, err := do()
	if err != nil {
		return data.Data, err
	}
	data.Hash = h
	data.Data = v

	b, err = json.Marshal(data)
	if err != nil {
		return data.Data, err
	}

	log.Tracef("hash writing: %v %s", h, b)

	err = os.WriteFile(path, b, os.ModePerm)
	if err != nil {
		return data.Data, err
	}

	return v, nil
}
