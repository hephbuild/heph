package pluginfs

import (
	"context"
	"fmt"
	"maps"
	"slices"

	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/pkg/xattr"
)

const (
	xattrCodegen       = "heph.codegen"
	xattrCodegenSource = "heph.codegen.source"
)

func IsCodegen(ctx context.Context, path string) bool {
	v, err := xattr.LGet(path, xattrCodegen)
	if err != nil {
		return false
	}

	return len(v) > 0
}

func MarkCodegen(ctx context.Context, source *pluginv1.TargetRef, to string) error {
	existingAttrs, err := xattr.LList(to)
	if err != nil {
		return fmt.Errorf("list xattrs: %w", err)
	}

	attrs := map[string]string{
		xattrCodegen:       "true",
		xattrCodegenSource: tref.Format(source),
	}

	hasAll := true
	for k := range maps.Keys(attrs) {
		if !slices.Contains(existingAttrs, k) {
			hasAll = false

			break
		}
	}

	if hasAll {
		return nil
	}

	for k, v := range attrs {
		err := xattr.LSet(to, k, []byte(v))
		if err != nil {
			return fmt.Errorf("set xattr %q: %w", k, err)
		}
	}

	return nil
}
