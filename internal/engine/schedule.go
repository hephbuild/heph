package engine

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/internal/herrgroup"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/metric"
	"go.opentelemetry.io/otel/trace"
)

func (e *Engine) Result(ctx context.Context, rs *RequestState, pkg, name string, outputs []string) (*ExecuteResultLocks, error) {
	res, err := e.ResultFromRef(ctx, rs, tref.New(pkg, name, nil), outputs)
	if err != nil {
		return nil, err
	}

	return res, nil
}

func (e *Engine) ResultFromRef(ctx context.Context, rs *RequestState, ref *pluginv1.TargetRef, outputs []string) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromRef", trace.WithAttributes(attribute.String("target", tref.Format(ref))))
	defer span.End()

	return e.result(ctx, rs, DefContainer{Ref: ref}, outputs, false)
}
func (e *Engine) ResultFromDef(ctx context.Context, rs *RequestState, def *TargetDef, outputs []string) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromDef", trace.WithAttributes(attribute.String("target", tref.Format(def.GetRef()))))
	defer span.End()

	return e.result(ctx, rs, DefContainer{Spec: def.TargetSpec, Def: def.TargetDef}, outputs, false)
}

func (e *Engine) ResultFromSpec(ctx context.Context, rs *RequestState, spec *pluginv1.TargetSpec, outputs []string) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromSpec", trace.WithAttributes(attribute.String("target", tref.Format(spec.GetRef()))))
	defer span.End()

	return e.result(ctx, rs, DefContainer{Spec: spec}, outputs, false)
}

type ExecuteResultsLocks []*ExecuteResultLocks

func (r ExecuteResultsLocks) Unlock(ctx context.Context) {
	for _, re := range r {
		re.Unlock(ctx)
	}
}

func (e *Engine) ResultsFromMatcher(ctx context.Context, rs *RequestState, matcher *pluginv1.TargetMatcher) (ExecuteResultsLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultsFromMatcher")
	defer span.End()

	var out ExecuteResultsLocks
	var outm sync.Mutex

	var matched bool
	g := herrgroup.NewContext(ctx, rs.FailFast)
	for ref, err := range e.Query(ctx, rs, matcher) {
		if err != nil {
			_ = g.Wait()

			out.Unlock(ctx)

			return nil, err
		}

		if g.Failed() {
			break
		}

		matched = true

		g.Go(func(ctx context.Context) error {
			res, err := e.ResultFromRef(ctx, rs, ref, []string{AllOutputs})
			if err != nil {
				return err
			}

			outm.Lock()
			out = append(out, res)
			outm.Unlock()

			return nil
		})
	}

	if !matched {
		return nil, errors.New("did not match any target")
	}

	err := g.Wait()
	if err != nil {
		out.Unlock(ctx)

		return nil, err
	}

	return out, nil
}

type Meta struct {
	Hashin string
}

func (e *Engine) meta(ctx context.Context, rs *RequestState, def *LightLinkedTarget) (*Meta, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("meta %v", tref.Format(def.GetRef())),
		}
	})
	defer cleanLabels()

	res, err, _ := rs.memMeta.Do(ctx, refKey(def.GetRef()), func(ctx context.Context) (*Meta, error) {
		manifests, err := e.depsResultMetas(ctx, rs, def)
		if err != nil {
			return nil, fmt.Errorf("deps manifests: %w", err)
		}

		hashin, err := e.hashin2(ctx, def, manifests, "meta")
		if err != nil {
			return nil, fmt.Errorf("hashin: %w", err)
		}

		return &Meta{
			Hashin: hashin,
		}, nil
	})

	return res, err
}

var meter = otel.Meter("heph_engine")

var resultCounter = sync.OnceValue(func() metric.Int64Counter {
	return htypes.Must2(meter.Int64Counter("result", metric.WithUnit("{count}")))
})

var providerGetHistogram = sync.OnceValue(func() metric.Float64Histogram {
	return htypes.Must2(meter.Float64Histogram(
		"provider.get",
		metric.WithUnit("s"),
		metric.WithExplicitBucketBoundaries(0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1, 2.5, 5, 7.5, 10),
	))
})

func (e *Engine) result(ctx context.Context, rs *RequestState, c DefContainer, outputs []string, onlyManifest bool) (*ExecuteResultLocks, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("Result %v", tref.Format(c.GetRef())),
		}
	})
	defer cleanLabels()

	rs, err := dagAddParent(rs, c.GetRef())
	if err != nil {
		return nil, err
	}

	clean := e.StoreRequestState(rs)
	defer clean()

	def, err := e.link(ctx, rs, c)
	if err != nil {
		return nil, fmt.Errorf("link: %v: %w", tref.Format(c.GetRef()), err)
	}

	ref := def.GetRef()

	switch {
	case onlyManifest:
		outputs = nil
	case len(outputs) == 1 && outputs[0] == AllOutputs:
		outputs = def.OutputNames()
	}

	var doOutputs []string
	switch {
	case def.GetCache():
		doOutputs = def.OutputNames()
	case len(outputs) == 1 && outputs[0] == AllOutputs:
		doOutputs = outputs
	default:
		doOutputs = outputs
	}

	meta, err := e.meta(ctx, rs, def)
	if err != nil {
		return nil, fmt.Errorf("meta: %w", err)
	}

	res, err, computed := rs.memResult.Do(ctx, keyRefOutputs(ref, doOutputs), func(ctx context.Context) (*ExecuteResultLocks, error) {
		resultCounter().Add(ctx, 1, metric.WithAttributes(
			attribute.String("target", tref.Format(ref)),
		))

		if e.RootSpan.IsRecording() {
			ctx = trace.ContextWithSpan(ctx, e.RootSpan)
		}
		ctx = hstep.WithoutParent(ctx)

		step, ctx := hstep.New(ctx, tref.Format(ref))
		defer step.Done()

		res, err := e.innerResultWithSideEffects(ctx, rs, def, doOutputs, meta)
		if err != nil {
			step.SetError()

			return nil, fmt.Errorf("%v: %w", tref.Format(ref), err)
		}

		return res, nil
	})
	if err != nil {
		return nil, err
	}

	if !computed {
		res = res.Clone()

		err := res.Locks.RLock(ctx)
		if err != nil {
			return nil, fmt.Errorf("lock: %w", err)
		}

		// e.ResultFromLocalCache(ctx, def, outputs, res.Hashin)

		// TODO: recheck if things are in cache, and if not, clear memResult and call it again
	} else {
		res = res.CloneWithoutLocks()
	}

	if onlyManifest {
		res.Artifacts = nil
	} else {
		res.Artifacts = slices.DeleteFunc(res.Artifacts, func(artifact *ResultArtifact) bool {
			if artifact.GetType() == pluginv1.Artifact_TYPE_SUPPORT_FILE {
				return false
			}

			if !slices.Contains(outputs, artifact.GetGroup()) {
				return true
			}

			return false
		})
	}

	return res, nil
}

type DepsResults []*ExecuteResultWithOrigin

func (r DepsResults) Unlock(ctx context.Context) {
	for _, res := range r {
		if res == nil {
			continue
		}

		res.Unlock(ctx)
	}
}

func (e *Engine) depsResults(ctx context.Context, rs *RequestState, t *LightLinkedTarget) (DepsResults, error) {
	ctx, span := tracer.Start(ctx, "depsResults", trace.WithAttributes(attribute.String("target", tref.Format(t.GetRef()))))
	defer span.End()

	if len(t.Inputs) == 0 {
		return nil, nil
	}

	inputs := slices.Clone(t.Inputs)
	slices.SortFunc(inputs, func(a, b *LightLinkedTargetInput) int {
		if v := tref.Compare(a.GetRef(), b.GetRef()); v != 0 {
			return v
		}

		return strings.Compare(a.Origin.GetId(), b.Origin.GetId())
	})

	g := herrgroup.NewContext(ctx, rs.FailFast)

	results := make(DepsResults, len(inputs))
	for i, dep := range inputs {
		g.Go(func(ctx context.Context) error {
			res, err := e.ResultFromDef(ctx, rs, dep.TargetDef, dep.Outputs)
			if err != nil {
				return err
			}

			res.Artifacts = slices.DeleteFunc(res.Artifacts, func(output *ResultArtifact) bool {
				return output.GetType() != pluginv1.Artifact_TYPE_OUTPUT && output.GetType() != pluginv1.Artifact_TYPE_SUPPORT_FILE
			})

			for _, artifact := range res.Artifacts {
				if artifact.GetHashout() == "" {
					return fmt.Errorf("%v: output %q has empty hashout", tref.Format(dep.GetRef()), artifact.GetGroup())
				}
			}

			results[i] = &ExecuteResultWithOrigin{
				ExecuteResultLocks: res,
				InputOrigin:        dep.Origin,
			}

			return nil
		})
	}

	err := g.Wait()
	if err != nil {
		results.Unlock(ctx)

		return nil, err
	}

	return results, nil
}

type DepMeta struct {
	Hashin    string
	Origin    *pluginv1.TargetDef_InputOrigin
	Artifacts []DepMetaArtifact
}

type DepMetaArtifact struct {
	Hashout string
}

type ResultMeta struct {
	Hashin    string
	Artifacts []ResultMetaArtifact
	CreatedAt time.Time
}

type ResultMetaArtifact struct {
	Hashout string
	Group   string
}

func (e *Engine) MetaFromRef(ctx context.Context, rs *RequestState, ref *pluginv1.TargetRef) (*Meta, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("MetaFromRef %v", tref.Format(ref)),
		}
	})
	defer cleanLabels()

	ctx, span := tracer.Start(ctx, "MetaFromRef", trace.WithAttributes(attribute.String("target", tref.Format(ref))))
	defer span.End()

	def, err := e.Link(ctx, rs, DefContainer{Ref: ref})
	if err != nil {
		return nil, err
	}

	meta, err := e.meta(ctx, rs, def)
	if err != nil {
		return nil, err
	}

	return meta, nil
}

func (e *Engine) ResultMetaFromRef(ctx context.Context, rs *RequestState, ref *pluginv1.TargetRef, outputs []string) (ResultMeta, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("ResultMetaFromRef %v", tref.Format(ref)),
		}
	})
	defer cleanLabels()

	ctx, span := tracer.Start(ctx, "ResultMetaFromRef", trace.WithAttributes(attribute.String("target", tref.Format(ref))))
	defer span.End()

	def, err := e.GetDef(ctx, rs, DefContainer{Ref: ref})
	if err != nil {
		return ResultMeta{}, err
	}

	return e.ResultMetaFromDef(ctx, rs, def, outputs)
}

func (e *Engine) ResultMetaFromDef(ctx context.Context, rs *RequestState, def *TargetDef, outputs []string) (ResultMeta, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("ResultMetaFromDef %v", tref.Format(def.GetRef())),
		}
	})
	defer cleanLabels()

	ctx, span := tracer.Start(ctx, "ResultMetaFromDef", trace.WithAttributes(attribute.String("target", tref.Format(def.GetRef()))))
	defer span.End()

	res, err := e.result(ctx, rs, DefContainer{Spec: def.TargetSpec, Def: def.TargetDef}, nil, true)
	if err != nil {
		return ResultMeta{}, fmt.Errorf("result: %w", err)
	}
	defer res.Unlock(ctx)

	manifest := res.Manifest

	m := ResultMeta{
		Hashin:    manifest.Hashin,
		CreatedAt: manifest.CreatedAt.UTC(),
	}

	if len(outputs) == 1 && outputs[0] == AllOutputs {
		outputs = def.OutputNames()
	}

	for _, artifact := range manifest.Artifacts {
		if !slices.Contains(outputs, artifact.Group) {
			continue
		}

		m.Artifacts = append(m.Artifacts, ResultMetaArtifact{
			Hashout: artifact.Hashout,
			Group:   artifact.Group,
		})
	}

	slices.SortFunc(m.Artifacts, func(a, b ResultMetaArtifact) int {
		return strings.Compare(a.Hashout, b.Hashout)
	})

	return m, nil
}

func (e *Engine) depsResultMetas(ctx context.Context, rs *RequestState, def *LightLinkedTarget) ([]DepMeta, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("depsResultMetas %v", tref.Format(def.GetRef())),
		}
	})
	defer cleanLabels()

	ctx, span := tracer.Start(ctx, "depsResultMetas", trace.WithAttributes(attribute.String("target", tref.Format(def.GetRef()))))
	defer span.End()

	if len(def.Inputs) == 0 {
		return nil, nil
	}

	inputs := slices.Clone(def.Inputs)
	slices.SortFunc(inputs, func(a, b *LightLinkedTargetInput) int {
		if v := tref.Compare(a.GetRef(), b.GetRef()); v != 0 {
			return v
		}

		return strings.Compare(a.Origin.GetId(), b.Origin.GetId())
	})

	g := herrgroup.NewContext(ctx, rs.FailFast)
	results := make([]DepMeta, len(inputs))

	for i, dep := range inputs {
		g.Go(func(ctx context.Context) error {
			res, err := e.ResultMetaFromDef(ctx, rs, dep.TargetDef, dep.Outputs)
			if err != nil {
				return fmt.Errorf("%v: %w", tref.Format(dep.GetRef()), err)
			}

			artifacts := make([]DepMetaArtifact, 0)
			for _, output := range dep.Outputs {
				for _, artifact := range res.Artifacts {
					if artifact.Group != output {
						continue
					}

					if artifact.Hashout == "" {
						return fmt.Errorf("%v: output %q has empty hashout", tref.Format(dep.GetRef()), output)
					}

					artifacts = append(artifacts, DepMetaArtifact{
						Hashout: artifact.Hashout,
					})
				}
			}

			slices.SortFunc(artifacts, func(a, b DepMetaArtifact) int {
				return strings.Compare(a.Hashout, b.Hashout)
			})

			results[i] = DepMeta{
				Hashin:    res.Hashin,
				Origin:    dep.Origin,
				Artifacts: artifacts,
			}

			return nil
		})
	}

	err := g.Wait()
	if err != nil {
		return nil, err
	}

	return results, nil
}

const AllOutputs = "__all_outputs__"

func (e *Engine) resultFromCache(ctx context.Context, rs *RequestState, def *LightLinkedTarget, outputs []string, hashin string) (*Result, bool, error) {
	res, ok, err := e.ResultFromLocalCache(ctx, def, outputs, hashin)
	if err != nil {
		return nil, false, fmt.Errorf("result from local cache: %w", err)
	}

	if ok {
		return res, true, nil
	}

	res, ok, err = e.ResultFromRemoteCache(ctx, rs, def, outputs, hashin)
	if err != nil {
		return nil, false, fmt.Errorf("result from remote cache: %w", err)
	}

	if ok {
		return res, true, nil
	}

	return nil, false, nil
}

func (e *Engine) innerResultWithSideEffects(ctx context.Context, rs *RequestState, def *LightLinkedTarget, outputs []string, meta *Meta) (*ExecuteResultLocks, error) {
	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("innerResultWithSideEffects %v", tref.Format(def.GetRef())),
		}
	})
	defer cleanLabels()

	res, err := e.innerResult(ctx, rs, def, outputs, meta)
	if err != nil {
		return nil, err
	}

	// TODO: move after execute
	err = e.codegenTree(ctx, res.Def, res.Artifacts)
	if err != nil {
		res.Unlock(ctx)

		return nil, fmt.Errorf("codegen tree: %w", err)
	}

	return res, nil
}

func (e *Engine) innerResult(ctx context.Context, rs *RequestState, def *LightLinkedTarget, outputs []string, meta *Meta) (*ExecuteResultLocks, error) {
	hashin := meta.Hashin

	shouldShell := tref.Equal(rs.Shell, def.GetRef())
	shouldForce := tmatch.MatchDef(def.TargetSpec, def.TargetDef.TargetDef, rs.Force) == tmatch.MatchYes
	getCache := def.GetCache() && !shouldShell && !shouldForce
	storeCache := def.GetCache() && !shouldShell

	if getCache {
		locks, err := e.lockCache(ctx, def.GetRef(), outputs, hashin, true)
		if err != nil {
			return nil, fmt.Errorf("lock cache: %w", err)
		}

		res, ok, err := e.resultFromCache(ctx, rs, def, outputs, hashin)
		if err != nil && !errors.Is(err, context.Canceled) {
			hlog.From(ctx).With(slog.String("target", tref.Format(def.GetRef())), slog.String("err", err.Error())).Warn("failed to get result from local cache")
		}

		if ok {
			return &ExecuteResultLocks{
				Result: res,
				Locks:  locks,
			}, nil
		}

		err = locks.Unlock()
		if err != nil {
			hlog.From(ctx).Error(fmt.Sprintf("%v: failed to unlock result locks: %v", tref.Format(def.GetRef()), err))
		}
	}

	res, err, computed := rs.memExecute.Do(ctx, refKey(def.GetRef()), func(ctx context.Context) (*ExecuteResultLocks, error) {
		if def.GetDriver() == plugingroup.Name {
			return e.resultGroup(ctx, rs, def, hashin)
		}

		locks, err := e.lockCache(ctx, def.GetRef(), outputs, hashin, false)
		if err != nil {
			return nil, fmt.Errorf("lock cache: %w", err)
		}

		execOptions := ExecuteOptions{
			shell:       shouldShell,
			force:       shouldForce,
			interactive: tref.Equal(rs.Shell, def.GetRef()) || tref.Equal(rs.Interactive, def.GetRef()),
			metaHashin:  hashin,
			getCache:    getCache,
			storeCache:  storeCache,
		}

		res, err := e.executeAndCacheInner(ctx, rs, def, execOptions)
		if err != nil {
			err = errors.Join(err, locks.Unlock())

			return nil, err
		}

		err = locks.Lock2RLock(ctx)
		if err != nil {
			err = errors.Join(err, locks.Unlock())

			return nil, err
		}

		return &ExecuteResultLocks{
			Result: res,
			Locks:  locks,
		}, nil
	})
	if err != nil {
		return nil, err
	}

	if !computed {
		res = res.Clone()

		err := res.Locks.RLock(ctx)
		if err != nil {
			err = errors.Join(err, res.Locks.Unlock())

			return nil, err
		}
	} else {
		res = res.CloneWithoutLocks()
	}

	return res, nil
}

var enableHashDebug = sync.OnceValue(func() bool {
	v, _ := strconv.ParseBool(os.Getenv("HEPH_DEBUG_HASH"))

	return v
})

type Result struct {
	Def       *LightLinkedTarget
	Hashin    string
	Artifacts []*ResultArtifact
	Manifest  *hartifact.Manifest
}

func (r Result) Sorted() *Result {
	slices.SortFunc(r.Artifacts, func(a, b *ResultArtifact) int {
		return strings.Compare(a.GetHashout(), b.GetHashout())
	})

	return &r
}

func (r Result) Clone() *Result {
	return &Result{
		Def:       r.Def,
		Hashin:    r.Hashin,
		Artifacts: slices.Clone(r.Artifacts),
		Manifest:  r.Manifest,
	}
}

func (r Result) FindOutputs(group string) []*ResultArtifact {
	res := make([]*ResultArtifact, 0, len(r.Artifacts))
	for _, artifact := range r.Artifacts {
		if artifact.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}
		if artifact.GetGroup() != group {
			continue
		}

		res = append(res, artifact)
	}

	return res
}

func (r Result) FindSupport() []*ResultArtifact {
	res := make([]*ResultArtifact, 0, len(r.Artifacts))
	for _, artifact := range r.Artifacts {
		if artifact.GetType() != pluginv1.Artifact_TYPE_SUPPORT_FILE {
			continue
		}

		res = append(res, artifact)
	}

	return res
}

type ResultArtifact struct {
	Manifest hartifact.ManifestArtifact

	pluginsdk.Artifact
}

func (r ResultArtifact) GetHashout() string {
	return r.Manifest.Hashout
}

func (e *Engine) executeAndCacheInner(ctx context.Context, rs *RequestState, def *LightLinkedTarget, options ExecuteOptions) (*Result, error) {
	results, err := e.depsResults(ctx, rs, def)
	if err != nil {
		return nil, fmt.Errorf("deps results: %w", err)
	}
	defer results.Unlock(ctx)

	hashin, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, fmt.Errorf("results hashin: %w", err)
	}

	if hashin != options.metaHashin {
		return nil, fmt.Errorf("results hashin (%v) != meta hashin (%v)", hashin, options.metaHashin)
	}

	cacheHashin := hashin
	if !options.storeCache {
		cacheHashin = hinstance.UID + "_" + hashin
	}

	if options.getCache {
		// One last cache check after the deps have completed
		res, ok, err := e.ResultFromLocalCache(ctx, def, def.OutputNames(), cacheHashin)
		if err != nil && !errors.Is(err, context.Canceled) {
			hlog.From(ctx).With(slog.String("target", tref.Format(def.GetRef())), slog.String("err", err.Error())).Warn("failed to get result from local cache")
		}

		if ok {
			return res, nil
		}
	}

	res, err := e.execute(ctx, rs, def, results, options)
	if err != nil {
		return nil, fmt.Errorf("execute: %w", err)
	}

	var artifactsToCache []*ExecuteArtifact
	var artifactsToPassthrough []*ResultArtifact
	if options.storeCache {
		artifactsToCache = res.Artifacts
	} else {
		// this caters for the pluginfs case where it doesnt make sense to copy the tree into the cache, if it's
		// an uncached target
		artifactsToPassthrough = make([]*ResultArtifact, 0, len(res.Artifacts))
		for _, artifact := range res.Artifacts {
			if e.isPassthroughArtifact(artifact) {
				m, err := hartifact.ProtoArtifactToManifest(artifact.Hashout, artifact)
				if err != nil {
					return nil, fmt.Errorf("protoartifacttomanifest: %w", err)
				}

				artifactsToPassthrough = append(artifactsToPassthrough, &ResultArtifact{
					Manifest: m,
					Artifact: artifact,
				})
			} else {
				artifactsToCache = append(artifactsToCache, artifact)
			}
		}
	}

	var cachedArtifacts []*ResultArtifact
	var manifest *hartifact.Manifest
	if !options.storeCache && len(artifactsToCache) == 0 {
		cachedArtifacts = artifactsToPassthrough

		manifest = &hartifact.Manifest{
			Version:   "v1",
			Target:    tref.Format(def.GetRef()),
			CreatedAt: time.Now(),
			Hashin:    hashin,
		}
		for _, artifact := range cachedArtifacts {
			martifact, err := hartifact.ProtoArtifactToManifest(artifact.GetHashout(), artifact.Artifact)
			if err != nil {
				return nil, err
			}

			manifest.Artifacts = append(manifest.Artifacts, martifact)
		}
	} else {
		cachedArtifacts, err = e.cacheLocally(ctx, def, cacheHashin, artifactsToCache)
		if err != nil {
			return nil, fmt.Errorf("cache locally: %w", err)
		}

		cachedArtifacts = append(cachedArtifacts, artifactsToPassthrough...)

		manifest, err = e.createLocalCacheManifest(ctx, def.GetRef(), cacheHashin, cachedArtifacts)
		if err != nil {
			return nil, fmt.Errorf("create local cache manifest: %w", err)
		}
	}

	if res.AfterCache != nil {
		res.AfterCache()
	}

	if options.storeCache {
		// TODO: move this to a background execution so that local build can proceed, while this is uploading in the background
		e.CacheRemotely(ctx, def, res.Hashin, manifest, cachedArtifacts)
	}

	return Result{
		Def:       def,
		Hashin:    res.Hashin,
		Artifacts: cachedArtifacts,
		Manifest:  manifest,
	}.Sorted(), nil
}

func (e *Engine) isPassthroughArtifact(artifact *ExecuteArtifact) bool {
	fsartifact, ok := artifact.Artifact.(pluginsdk.FSArtifact)
	if !ok {
		return false
	}

	node := fsartifact.FSNode()
	if node == nil {
		return false
	}

	if hfs.HasPathPrefix(node.Path(), e.Home.Path()) {
		return false
	}

	return true
}
