package engine

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/hephbuild/heph/tmatch"
	"iter"
	"path/filepath"
	"slices"
)

func (e *Engine) Packages(ctx context.Context, matcher *pluginv1.TargetMatcher) iter.Seq2[string, error] {
	return tmatch.Packages(e.Root.Path(), matcher, func(path string) bool {
		if path == e.Home.Path() {
			return false
		}

		// TODO: parametrize
		if slices.Contains([]string{"node_modules", "dist"}, filepath.Base(path)) {
			return false
		}

		return true
	})
}

func (e *Engine) listDeep(ctx context.Context, p Provider, pkg string, seen map[string]struct{}) iter.Seq2[*pluginv1.TargetSpec, error] {
	key := p.Name + " " + pkg
	if _, ok := seen[key]; ok {
		return func(yield func(*pluginv1.TargetSpec, error) bool) {}
	}
	seen[key] = struct{}{}

	return func(yield func(*pluginv1.TargetSpec, error) bool) {
		res, err := p.List(ctx, connect.NewRequest(&pluginv1.ListRequest{
			Package: pkg,
		}))
		if err != nil {
			yield(nil, err)
			return
		}
		defer res.Close()

		for res.Receive() {
			ref := res.Msg().GetRef()
			if ref == nil {
				ref = res.Msg().GetSpec().Ref
			}

			def, err := e.Link(ctx, DefContainer{Ref: ref})
			if err != nil {
				yield(nil, err)
				return
			}

			if !yield(def.TargetSpec, nil) {
				return
			}

			for _, input := range def.Inputs {
				for ref, err := range e.listDeep(ctx, p, input.GetRef().GetPackage(), seen) {
					if !yield(ref, err) {
						return
					}
				}
			}
		}
		if res.Err() != nil {
			yield(nil, res.Err())
			return
		}
	}
}

func (e *Engine) Query(ctx context.Context, matcher *pluginv1.TargetMatcher) iter.Seq2[*pluginv1.TargetRef, error] {
	return func(yield func(*pluginv1.TargetRef, error) bool) {
		seenPkg := map[string]struct{}{}
		seenRef := map[string]struct{}{}

		for pkg, err := range e.Packages(ctx, matcher) {
			if err != nil {
				yield(nil, err)
				return
			}

			for _, provider := range e.Providers {
				for spec, err := range e.listDeep(ctx, provider, pkg, seenPkg) {
					if err != nil {
						hlog.From(ctx).Error("error listing deep", "provider", provider.Name, "spec", spec, "err", err)
						continue
					}

					ref := spec.Ref

					if _, ok := seenRef[tref.Format(ref)]; ok {
						continue
					}
					seenRef[tref.Format(ref)] = struct{}{}

					if tmatch.MatchSpec(spec, matcher) != tmatch.MatchYes {
						continue
					}

					if !yield(ref, err) {
						return
					}
				}
			}
		}
	}
}
