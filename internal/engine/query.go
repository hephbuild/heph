package engine

import (
	"context"
	"fmt"
	"iter"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func (e *Engine) Packages(ctx context.Context, matcher *pluginv1.TargetMatcher) iter.Seq2[string, error] {
	var provider tmatch.PackageProvider
	if e.WellKnownPackages != nil {
		provider = func(ctx context.Context, basePkg string) iter.Seq2[string, error] {
			return func(yield func(string, error) bool) {
				for _, pkg := range e.WellKnownPackages {
					if !yield(pkg, nil) {
						return
					}
				}
			}
		}
	} else {
		provider = tmatch.OSPackageProvider(e.Root.Path(), func(path string) bool {
			if path == e.Home.Path() {
				return false
			}

			if ok, _ := hfs.PathMatchAny(path, e.PackagesExlude...); ok {
				return false
			}

			return true
		})
	}

	return tmatch.Packages(ctx, provider, matcher)
}

func (e *Engine) queryListProvider(
	ctx context.Context,
	rs *RequestState,
	p EngineProvider,
	pkg string,
	seen map[seenPkgKey]struct{},
) iter.Seq2[*pluginv1.TargetRef, error] {
	key := seenPkgKey{
		pname: p.Name,
		pkg:   pkg,
	}
	if _, ok := seen[key]; ok {
		return func(yield func(*pluginv1.TargetRef, error) bool) {}
	}
	seen[key] = struct{}{}

	return func(yield func(*pluginv1.TargetRef, error) bool) {
		res, err := e.List(ctx, rs, p, pkg)
		if err != nil {
			yield(nil, err)
			return
		}
		defer res.CloseReceive() //nolint:errcheck

		for res.Receive() {
			msg := res.Msg()

			ref := msg.GetRef()
			if ref == nil {
				ref = msg.GetSpec().GetRef()
			}

			if !yield(ref, nil) {
				return
			}
		}
		if res.Err() != nil {
			yield(nil, res.Err())
			return
		}
	}
}

type seenPkgKey struct {
	pname string
	pkg   string
}

func (e *Engine) match(ctx context.Context, rs *RequestState, ref *pluginv1.TargetRef, matcher *pluginv1.TargetMatcher, opts queryOptions) (tmatch.Result, error) {
	if r := tmatch.MatchPackage(ref.GetPackage(), matcher); r.Definitive() {
		return r, nil
	}

	spec, err := e.GetSpec(ctx, rs, SpecContainer{Ref: ref})
	if err != nil {
		return 0, err
	}

	if r := tmatch.MatchSpec(spec, matcher); r.Definitive() {
		return r, nil
	}

	def, err := e.GetDef(ctx, rs, DefContainer{Ref: ref, Spec: spec})
	if err != nil {
		return 0, err
	}

	if r := tmatch.MatchDef(spec, def.TargetDef, matcher); r.Definitive() {
		return r, nil
	}

	return tmatch.MatchShrug, nil
}

type queryOptions struct {
	filterProvider func(p EngineProvider) bool
}

func (e *Engine) query(ctx context.Context, rs *RequestState, matcher *pluginv1.TargetMatcher, opts queryOptions) iter.Seq2[*pluginv1.TargetRef, error] {
	if opts.filterProvider == nil {
		opts.filterProvider = func(p EngineProvider) bool {
			return true
		}
	}

	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("Query %v", matcher.String()),
		}
	})
	defer cleanLabels()

	if matcher.HasRef() {
		ref := matcher.GetRef()
		return func(yield func(*pluginv1.TargetRef, error) bool) {
			spec, err := e.GetSpec(ctx, rs, SpecContainer{Ref: ref}) // check if exist
			yield(spec.GetRef(), err)
		}
	}

	return func(yield func(*pluginv1.TargetRef, error) bool) {
		seenPkg := map[seenPkgKey]struct{}{}
		seenRef := map[string]struct{}{}

		for pkg, err := range e.Packages(ctx, matcher) {
			if err != nil {
				yield(nil, err)
				return
			}

			for _, provider := range e.Providers {
				if !opts.filterProvider(provider) {
					continue
				}

				for ref, err := range e.queryListProvider(ctx, rs, provider, pkg, seenPkg) {
					if err != nil {
						//if errors.Is(err, StackRecursionError{}) {
						//	continue
						//}

						if err != nil {
							yield(nil, err)
							return
						}

						hlog.From(ctx).Error("failed query", "pkg", pkg, "provider", provider.Name, "err", err)
						continue
					}

					refstr := tref.Format(ref)

					if _, ok := seenRef[refstr]; ok {
						continue
					}
					seenRef[refstr] = struct{}{}

					res, err := e.match(ctx, rs, ref, matcher, opts)
					if err != nil {
						yield(nil, err)
						return
					}

					if res == tmatch.MatchNo {
						continue
					}

					if !yield(ref, nil) {
						return
					}
				}
			}
		}
	}
}

func (e *Engine) Query(ctx context.Context, rs *RequestState, matcher *pluginv1.TargetMatcher) iter.Seq2[*pluginv1.TargetRef, error] {
	return e.query(ctx, rs, matcher, queryOptions{})
}
