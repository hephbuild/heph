package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/hephbuild/heph/tmatch"
	sync_map "github.com/zolstein/sync-map"
	"golang.org/x/sync/semaphore"
	"iter"
	"path/filepath"
	"slices"
	"sync"
)

func (e *Engine) Packages(ctx context.Context, matcher *pluginv1.TargetMatcher) iter.Seq2[string, error] {
	return tmatch.Packages(ctx, e.Root.Path(), matcher, func(path string) bool {
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

func (e *Engine) queryListProvider(ctx context.Context, p EngineProvider, pkg string, seen map[seenPkgKey]struct{}, rs *RequestState) iter.Seq2[*pluginv1.TargetSpec, error] {
	key := seenPkgKey{
		pname: p.Name,
		pkg:   pkg,
	}
	if _, ok := seen[key]; ok {
		return func(yield func(*pluginv1.TargetSpec, error) bool) {}
	}
	seen[key] = struct{}{}

	return func(yield func(*pluginv1.TargetSpec, error) bool) {
		res, err := e.List(ctx, p, pkg, rs)
		if err != nil {
			if errors.Is(err, ErrStackRecursion{}) {
				return
			}

			yield(nil, err)
			return
		}
		defer res.CloseReceive()

		for res.Receive() {
			msg := res.Msg()

			def, err := e.GetDef(ctx, DefContainer{Ref: msg.GetRef(), Spec: msg.GetSpec()}, rs)
			if err != nil {
				yield(nil, err)
				return
			}

			if !yield(def.TargetSpec, nil) {
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

func (e *Engine) match(ctx context.Context, ref *pluginv1.TargetRef, matcher *pluginv1.TargetMatcher, rs *RequestState) (tmatch.Result, error) {
	if r := tmatch.MatchPackage(ref.Package, matcher); r.Definitive() {
		return r, nil
	}

	spec, err := e.GetSpec(ctx, SpecContainer{Ref: ref}, rs)
	if err != nil {
		return 0, err
	}

	if r := tmatch.MatchSpec(spec, matcher); r.Definitive() {
		return r, nil
	}

	def, err := e.GetDef(ctx, DefContainer{Ref: ref, Spec: spec}, rs)
	if err != nil {
		return 0, err
	}

	if r := tmatch.MatchDef(spec, def.TargetDef, matcher); r.Definitive() {
		return r, nil
	}

	return tmatch.MatchShrug, nil
}

func (e *Engine) query1(ctx context.Context, matcher *pluginv1.TargetMatcher, rs *RequestState) iter.Seq2[*pluginv1.TargetRef, error] {
	return func(yield func(*pluginv1.TargetRef, error) bool) {
		seenPkg := map[seenPkgKey]struct{}{}
		seenRef := map[string]struct{}{}

		for pkg, err := range e.Packages(ctx, matcher) {
			if err != nil {
				yield(nil, err)
				return
			}

			if tmatch.MatchPackage(pkg, matcher) == tmatch.MatchNo {
				continue
			}

			for _, provider := range e.Providers {
				for spec, err := range e.queryListProvider(ctx, provider, pkg, seenPkg, rs) {
					if err != nil {
						if errors.Is(err, ErrStackRecursion{}) {
							continue
						}

						hlog.From(ctx).Error("failed query", "pkg", pkg, "provider", provider.Name, "err", err)
						continue
					}

					ref := spec.Ref
					refstr := tref.Format(ref)

					if _, ok := seenRef[refstr]; ok {
						continue
					}
					seenRef[refstr] = struct{}{}

					res, err := e.match(ctx, ref, matcher, rs)
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

type queryState struct {
	*Engine
	seenPkg sync_map.Map[string, struct{}]
	seenRef sync_map.Map[string, struct{}]
	ch      chan queryStateRes
	wg      sync.WaitGroup
	listSem *semaphore.Weighted
	rs      *RequestState
}

type queryStateRes struct {
	spec *pluginv1.TargetSpec
	err  error
}

func (e *queryState) sendSpec(ctx context.Context, spec *pluginv1.TargetSpec) {
	select {
	case <-ctx.Done():
	case e.ch <- queryStateRes{spec: spec}:
	}
}

func (e *queryState) sendErr(ctx context.Context, err error) {
	select {
	case <-ctx.Done():
	case e.ch <- queryStateRes{err: err}:
	}
}

func (e *queryState) queryPackage(ctx context.Context, pkg string) {
	if _, loaded := e.seenPkg.LoadOrStore(pkg, struct{}{}); loaded {
		return
	}

	for _, provider := range e.Providers {
		e.wg.Add(1)

		go func() {
			defer e.wg.Done()

			e.queryListProvider(ctx, provider, pkg)
		}()
	}
}

func (e *queryState) queryListProvider(ctx context.Context, p EngineProvider, pkg string) {
	err := e.listSem.Acquire(ctx, 1)
	if err != nil {
		e.sendErr(ctx, err)
		return
	}
	defer e.listSem.Release(1)

	res, err := e.List(ctx, p, pkg, e.rs)
	if err != nil {
		e.sendErr(ctx, fmt.Errorf("%v list: %w", p.Name, err))
		return
	}
	defer res.CloseReceive()

	for res.Receive() {
		msg := res.Msg()
		e.handleRefSpec(ctx, msg.GetRef(), msg.GetSpec())
	}
	if err := res.Err(); err != nil {
		e.sendErr(ctx, fmt.Errorf("%v list: %w", p.Name, err))
		return
	}
}

func (e *queryState) handleRefSpec(ctx context.Context, ref *pluginv1.TargetRef, spec *pluginv1.TargetSpec) {
	if _, loaded := e.seenRef.LoadOrStore(tref.Format(ref), struct{}{}); loaded {
		return
	}

	def, err := e.GetDef(ctx, DefContainer{Ref: ref, Spec: spec}, e.rs)
	if err != nil {
		e.sendErr(ctx, fmt.Errorf("get def: %w", err))
		return
	}

	e.sendSpec(ctx, def.TargetSpec)

	for _, input := range def.GetInputs() {
		e.handleRefSpec(ctx, tref.WithoutOut(input.Ref), nil)
	}
}

func (e *queryState) query2(ctx context.Context, matcher *pluginv1.TargetMatcher) iter.Seq2[*pluginv1.TargetRef, error] {
	return func(yield func(*pluginv1.TargetRef, error) bool) {
		ctx, cancel := context.WithCancel(ctx)
		defer cancel()

		for pkg, err := range e.Packages(ctx, matcher) {
			if err != nil {
				yield(nil, err)
				return
			}

			e.queryPackage(ctx, pkg)
		}

		go func() {
			e.wg.Wait()
			cancel()
			close(e.ch)
		}()

		seenRef := map[string]struct{}{}

		for res := range e.ch {
			if res.err != nil {
				if !errors.Is(res.err, context.Canceled) {
					hlog.From(ctx).Error("failed query", "err", res.err)
				}
				continue
			}

			spec := res.spec
			ref := spec.Ref

			if _, ok := seenRef[tref.Format(ref)]; ok {
				continue
			}
			seenRef[tref.Format(ref)] = struct{}{}

			res, err := e.match(ctx, ref, matcher, e.rs)
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

func (e *Engine) Query(ctx context.Context, matcher *pluginv1.TargetMatcher, rs *RequestState) iter.Seq2[*pluginv1.TargetRef, error] {
	if matcher, ok := matcher.Item.(*pluginv1.TargetMatcher_Ref); ok {
		return func(yield func(*pluginv1.TargetRef, error) bool) {
			spec, err := e.GetSpec(ctx, SpecContainer{Ref: matcher.Ref}, rs)
			yield(spec.Ref, err)
		}
	}

	if false {
		state := &queryState{
			Engine:  e,
			rs:      rs,
			ch:      make(chan queryStateRes, 1000),
			listSem: semaphore.NewWeighted(100),
		}
		return state.query2(ctx, matcher)
	} else {
		return e.query1(ctx, matcher, rs)
	}
}
