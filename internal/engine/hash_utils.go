package engine

import (
	"encoding/hex"
	"encoding/json"
	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hproto"
	"github.com/zeebo/xxh3"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
	"hash"
	"os"
	"path/filepath"
	"strconv"
	"sync/atomic"
)

// Useful for figuring out why hash isnt deterministic

type hashWithDebug struct {
	*xxh3.Hasher
	path string
}

var debugCounter = cache.New[string, *atomic.Int32]()

func newHashWithDebug(w *xxh3.Hasher, name string) hashWithDebug {
	c, _ := debugCounter.GetOrSet(name, &atomic.Int32{})
	id := c.Add(1)

	path := filepath.Join("/tmp/hashdebug", hinstance.UID, name, strconv.Itoa(int(id))+".txt")

	return hashWithDebug{Hasher: w, path: path}
}

func (h hashWithDebug) WriteString(s string) (int, error) {
	return h.Write([]byte(s))
}

func (h hashWithDebug) Write(p []byte) (int, error) {
	err := os.MkdirAll(filepath.Dir(h.path), 0777)
	if err != nil {
		return 0, err
	}

	f, err := os.OpenFile(h.path, os.O_RDWR|os.O_CREATE|os.O_APPEND, 0777)
	if err != nil {
		return 0, err
	}
	defer f.Close()

	_, err = f.Write(p)
	if err != nil {
		return 0, err
	}
	_, err = f.WriteString("\n")
	if err != nil {
		return 0, err
	}

	return h.Hasher.Write(p)
}

func (h hashWithDebug) Sum(b []byte) []byte {
	sum := h.Hasher.Sum(b)

	_, _ = h.WriteString("SUM: " + hex.EncodeToString(sum))

	return sum
}

func stableProtoHashEncode(w hash.Hash, v proto.Message) error {
	if v, ok := v.(hproto.Hashable); ok {
		v.HashPB(w, nil)

		return nil
	}

	// this is pretty damn inefficient, but at least its stable
	b, err := protojson.Marshal(v)
	if err != nil {
		return err
	}

	var a any
	err = json.Unmarshal(b, &a)
	if err != nil {
		return err
	}

	return json.NewEncoder(w).Encode(a)
}
