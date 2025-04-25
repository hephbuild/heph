package engine

import (
	"encoding/json"
	"github.com/hephbuild/heph/internal/hproto"
	"hash"
	"os"
	"path/filepath"
	"time"

	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/zeebo/xxh3"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

// Useful for figuring out why hash isnt deterministic

type hashWithDebug struct {
	*xxh3.Hasher
	path string
}

func newHashWithDebug(w *xxh3.Hasher, name string) hashWithDebug {
	path := filepath.Join("/tmp/hashdebug", hinstance.UID, name, time.Now().Format(time.StampNano)+".txt")
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

// TODO: this is pretty damn inefficient, but at least its stable
func stableProtoHashEncode(w hash.Hash, v proto.Message, ignore map[string]struct{}) error {
	if v, ok := v.(hproto.Hashable); ok {
		v.HashPB(w, ignore)

		return nil
	}

	v, err := hproto.RemoveMasked(v, ignore)
	if err != nil {
		return err
	}

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
