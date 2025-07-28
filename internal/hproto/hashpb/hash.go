package hashpb

import (
	"encoding/json"
	"hash"
	"io"

	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
)

// Hashable matches https://github.com/cerbos/protoc-gen-go-hashpb/blob/db0168880c5d9ad459ff3be9157f7f4eac77412c/internal/generator/generator_test.go#L24
type Hashable interface {
	proto.Message
	HashPB(hash.Hash, map[string]struct{})
}

type StableWriter interface {
	proto.Message
	StableWrite(w io.Writer, ignore map[string]struct{}) error
}

func Hash(h hash.Hash, msg StableWriter, omit map[string]struct{}) {
	_ = msg.StableWrite(h, omit)
}

func HashMessage(h hash.Hash, msg proto.Message) error {
	if v, ok := msg.(StableWriter); ok {
		Hash(h, v, nil)

		return nil
	}

	// this is pretty damn inefficient, but at least its stable
	b, err := protojson.Marshal(msg)
	if err != nil {
		return err
	}

	var a any
	err = json.Unmarshal(b, &a)
	if err != nil {
		return err
	}

	err = json.NewEncoder(h).Encode(a)
	if err != nil {
		return err
	}

	return nil
}
