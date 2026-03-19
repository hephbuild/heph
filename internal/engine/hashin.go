package engine

import (
	"context"
	"encoding/hex"
	"errors"
	"hash"
	"io"
	"slices"
	"strings"

	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/lib/tref"
	"github.com/zeebo/xxh3"
)

func (e *Engine) hashin2(ctx context.Context, def *LightLinkedTarget, results []DepMeta, debugHint string) (string, error) {
	var h interface {
		hash.Hash
		io.StringWriter
	}
	if enableHashDebug() {
		h = newHashWithDebug(xxh3.New(), strings.TrimPrefix(tref.Format(def.GetRef()), "//"), debugHint)
	} else {
		h = xxh3.New()
	}
	writeProto := func(v hashpb.StableWriter) error {
		hashpb.Hash(h, v, tref.OmitHashPb)

		return nil
	}

	err := writeProto(def.GetRef())
	if err != nil {
		return "", err
	}

	if len(def.GetHash()) == 0 {
		return "", errors.New("hash is empty")
	}

	_, err = h.Write(def.GetHash())
	if err != nil {
		return "", err
	}

	// TODO support fieldmask of deps to include in hashin
	for _, result := range results {
		_, err = h.WriteString(result.Origin.GetId())
		if err != nil {
			return "", err
		}

		for _, output := range result.Artifacts {
			_, err = h.WriteString(output.Hashout)
			if err != nil {
				return "", err
			}
		}
	}

	hashin := hex.EncodeToString(h.Sum(nil))

	return hashin, nil
}

func (e *Engine) hashin(ctx context.Context, def *LightLinkedTarget, results []*ExecuteResultWithOrigin) (string, error) {
	metas := make([]DepMeta, 0, len(results))

	for _, result := range results {
		artifacts := make([]DepMetaArtifact, 0, len(result.Artifacts))
		for _, artifact := range result.Artifacts {
			artifacts = append(artifacts, DepMetaArtifact{
				Hashout: artifact.GetHashout(),
			})
		}

		slices.SortFunc(artifacts, func(a, b DepMetaArtifact) int {
			return strings.Compare(a.Hashout, b.Hashout)
		})

		metas = append(metas, DepMeta{
			Hashin:    result.Hashin,
			Origin:    result.InputOrigin,
			Artifacts: artifacts,
		})
	}

	return e.hashin2(ctx, def, metas, "res")
}
