package engine

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/internal/herrgroup"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hpanic"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/hpipe"
	"github.com/hephbuild/heph/lib/pluginsdk"
	"github.com/hephbuild/heph/lib/tref"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/metric"
	"golang.org/x/sync/semaphore"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hpty"
	"github.com/hephbuild/heph/internal/htar"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"google.golang.org/protobuf/proto"
)

type ExecOptions struct {
	Stdin  io.Reader
	Stdout io.Writer
	Stderr io.Writer
}

type ExecuteOptions struct {
	shell       bool
	force       bool
	interactive bool
	metaHashin  string
	getCache    bool
	storeCache  bool
}

type InteractiveExecOptions struct {
	Run func(context.Context, ExecOptions)
	Pty bool
}

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

	res, err, computed := rs.memResult.Do(ctx, keyRefOutputs(ref, doOutputs)+meta.Hashin, func(ctx context.Context) (*ExecuteResultLocks, error) {
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
		CreatedAt: manifest.CreatedAt,
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
			results, err := e.depsResults(ctx, rs, def)
			if err != nil {
				return nil, fmt.Errorf("deps results: %w", err)
			}
			defer results.Unlock(ctx)

			manifest := &hartifact.Manifest{
				Version:   "v1",
				Target:    tref.Format(def.GetRef()),
				CreatedAt: time.Now(),
				Hashin:    hashin,
			}

			var artifacts []*ResultArtifact
			var locks CacheLocks
			for _, result := range results {
				for _, artifact := range result.Artifacts {
					partifact := artifactGroupMap{Artifact: artifact, group: ""} // TODO support output group

					martifact, err := hartifact.ProtoArtifactToManifest(artifact.GetHashout(), partifact)
					if err != nil {
						return nil, fmt.Errorf("proto artifact to manifest: %w", err)
					}

					artifacts = append(artifacts, &ResultArtifact{
						Artifact: partifact,
						Manifest: martifact,
					})

					manifest.Artifacts = append(manifest.Artifacts, martifact)

					locks.AddFrom(result.Locks)
				}
			}

			err = locks.RLock(ctx)
			if err != nil {
				return nil, err
			}

			return &ExecuteResultLocks{
				Result: Result{
					Def:       def,
					Hashin:    hashin,
					Artifacts: artifacts,
					Manifest:  manifest,
				}.Sorted(),
				Locks: &locks,
			}, nil
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

	if !def.GetCache() {
		_, err = h.WriteString(hinstance.UID)
		if err != nil {
			return "", err
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

type ExecuteResult struct {
	Def        *LightLinkedTarget
	Hashin     string
	Artifacts  []*ExecuteArtifact
	AfterCache func()
}

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

type ExecuteArtifact struct {
	Hashout string
	pluginsdk.Artifact
}

type ResultArtifact struct {
	Manifest hartifact.ManifestArtifact

	pluginsdk.Artifact
}

func (r ResultArtifact) GetHashout() string {
	return r.Manifest.Hashout
}

type ExecuteResultLocks struct {
	*Result
	Locks *CacheLocks
}

func (r *ExecuteResultLocks) Clone() *ExecuteResultLocks {
	return &ExecuteResultLocks{
		Result: r.Result.Clone(),
		Locks:  r.Locks.Clone(),
	}
}

func (r *ExecuteResultLocks) CloneWithoutLocks() *ExecuteResultLocks {
	return &ExecuteResultLocks{
		Result: r.Result.Clone(),
		Locks:  r.Locks,
	}
}

func (r *ExecuteResultLocks) Unlock(ctx context.Context) {
	if r == nil {
		return
	}

	err := r.Locks.Unlock()
	if err != nil {
		hlog.From(ctx).Error(fmt.Sprintf("%v: failed to unlock result locks: %v", tref.Format(r.Def.GetRef()), err))
	}
}

func (r ExecuteResult) Sorted() *ExecuteResult {
	slices.SortFunc(r.Artifacts, func(a, b *ExecuteArtifact) int {
		return strings.Compare(a.Hashout, b.Hashout)
	})

	return &r
}

func (r ExecuteResult) Clone() *ExecuteResult {
	return &ExecuteResult{
		Def:        r.Def.Clone(),
		Hashin:     r.Hashin,
		Artifacts:  slices.Clone(r.Artifacts),
		AfterCache: r.AfterCache,
	}
}

type ExecuteResultWithOrigin struct {
	*ExecuteResultLocks
	InputOrigin *pluginv1.TargetDef_InputOrigin
}

func (e *Engine) pipes(ctx context.Context, rs *RequestState, driver pluginsdk.Driver, options ExecOptions) ([]string, func() error, error) {
	pipes := []string{"", "", "", "", ""}
	var eg herrgroup.Group

	var cancels []func()
	var stdinErrCh chan error

	wait := func() error {
		for _, cancel := range cancels {
			cancel()
		}

		err := eg.Wait()

		// this is complicated...
		// if the stdin connected was stdin, and we exec a command, stdin never closes, but the Read interface
		// doesnt have a way to stop reading based on context cancellation, so this just hangs until there is a write into stdin,
		// which makes Read return only to realise that the writer is gone and error out with io: read/write on closed pipe
		// TODO: explore https://github.com/muesli/cancelreader
		if false && stdinErrCh != nil {
			err = errors.Join(err, <-stdinErrCh)
		}

		if errors.Is(err, context.Canceled) {
			return nil
		}

		return err
	}

	driverHandle := e.DriversHandle[driver]

	if options.Stdin != nil {
		stdinErrCh = make(chan error)

		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}

		if res != nil && res.GetId() != "" {
			pipes[0] = res.GetId()

			ctx, cancel := context.WithCancel(ctx)
			cancels = append(cancels, cancel)

			go func() {
				defer cancel()

				w, err := hpipe.Writer(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					stdinErrCh <- err
					return
				}
				defer w.Close()

				_, err = io.Copy(w, options.Stdin)

				stdinErrCh <- err
			}()
		}
	}

	if options.Stdout != nil {
		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}

		if res != nil && res.GetId() != "" {
			pipes[1] = res.GetId()

			eg.Go(func() error {
				r, err := hpipe.Reader(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					return err
				}

				_, err = io.Copy(options.Stdout, r)

				return err
			})
		}
	}

	if options.Stderr != nil {
		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}

		if res != nil && res.GetId() != "" {
			pipes[2] = res.GetId()

			eg.Go(func() error {
				r, err := hpipe.Reader(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					return err
				}

				_, err = io.Copy(options.Stderr, r)

				return err
			})
		}
	}

	if stdin, ok := options.Stdin.(*os.File); ok {
		res, err := driver.Pipe(ctx, pluginv1.PipeRequest_builder{
			RequestId: htypes.Ptr(rs.ID),
		}.Build())
		if err != nil && !errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, wait, err
		}
		if res != nil && res.GetId() != "" {
			pipes[3] = res.GetId()

			ch, clean := hpty.WinSizeChan(ctx, stdin)
			cancels = append(cancels, clean)

			go func() {
				w, err := hpipe.Writer(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.GetPath())
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("failed to get pipe: %v", err))
					return
				}
				defer w.Close()

				for size := range ch {
					b, err := json.Marshal(size)
					if err != nil {
						hlog.From(ctx).Error(fmt.Sprintf("failed to marshal size: %v", err))
						continue
					}

					_, _ = w.Write(b)
					_, _ = w.Write([]byte("\n"))
				}
			}()
		}
	}

	return pipes, wait, nil
}

func (e *Engine) pickShellDriver(ctx context.Context, def *LightLinkedTarget) (pluginsdk.Driver, error) {
	var errs error
	for _, shellDriver := range []string{def.GetDriver() + "@shell"} {
		driver, ok := e.DriversByName[shellDriver]
		if ok {
			return driver, nil
		} else {
			errs = errors.Join(fmt.Errorf("shell driver not found: %v", shellDriver))
		}
	}

	if errs == nil {
		errs = errors.New("no shell driver found")
	}

	return nil, errs
}

var sem = semaphore.NewWeighted(1000 * int64(runtime.GOMAXPROCS(-1)))

func (e *Engine) execute(ctx context.Context, rs *RequestState, def *LightLinkedTarget, results DepsResults, options ExecuteOptions) (*ExecuteResult, error) {
	hashin := options.metaHashin

	ctx, span := tracer.Start(ctx, "Execute")
	defer span.End()

	ctx, cleanLabels := hdebug.SetLabels(ctx, func() []string {
		return []string{
			"where", fmt.Sprintf("Execute %v", tref.Format(def.GetRef())),
		}
	})
	defer cleanLabels()

	driver, ok := e.DriversByName[def.GetDriver()]
	if !ok {
		return nil, fmt.Errorf("driver not found: %v", def.GetDriver())
	}

	if options.shell {
		var err error
		driver, err = e.pickShellDriver(ctx, def)
		if err != nil {
			return nil, err
		}
		def.SetPty(true)
	}

	var targetfolder string
	if def.GetCache() || options.shell {
		targetfolder = e.targetDirName(def.GetRef())
	} else {
		targetfolder = fmt.Sprintf("__%v__%v", e.targetDirName(def.GetRef()), time.Now().UnixNano())
	}

	err := sem.Acquire(ctx, 1)
	if err != nil {
		return nil, err
	}
	defer sem.Release(1)

	step, ctx := hstep.New(ctx, "Running...")
	defer step.Done()

	sandboxfs := hfs.At(e.Sandbox, def.GetRef().GetPackage(), targetfolder)
	workdirfs := hfs.At(sandboxfs, "ws") // TODO: remove the ws from here
	cwdfs := hfs.At(workdirfs, def.GetRef().GetPackage())

	err = sandboxfs.RemoveAll()
	if err != nil {
		return nil, err
	}

	err = sandboxfs.MkdirAll(os.ModePerm)
	if err != nil {
		return nil, err
	}

	execWrapper := func(ctx context.Context, args InteractiveExecOptions) error {
		args.Run(ctx, ExecOptions{})
		return nil
	}

	if options.interactive {
		execWrapper = rs.InteractiveExec
		if execWrapper == nil {
			return nil, errors.New("interactive mode requires interactiveExec")
		}
	}

	var inputLen int
	for _, result := range results {
		inputLen += len(result.Artifacts)
	}

	inputs := make([]*pluginv1.RunRequest_Input, 0, inputLen)
	inputsSdk := make([]*pluginsdk.ArtifactWithOrigin, 0, inputLen)
	for _, result := range results {
		for _, artifact := range result.Artifacts {
			partifact := pluginv1.RunRequest_Input_Artifact_builder{
				Group: htypes.Ptr(artifact.GetGroup()),
				Name:  htypes.Ptr(artifact.GetName()),
				Type:  htypes.Ptr(artifact.GetType()),
				Id:    htypes.Ptr("TODO"), // to be implemented along with the pluginsdkconnect.ProtoArtifact
			}.Build()

			inputs = append(inputs, pluginv1.RunRequest_Input_builder{
				Artifact: partifact,
				Origin:   result.InputOrigin,
			}.Build())

			inputsSdk = append(inputsSdk, &pluginsdk.ArtifactWithOrigin{
				Artifact: artifact.Artifact,
				Origin:   result.InputOrigin,
			})
		}
	}

	var runRes *pluginv1.RunResponse
	var runErr error
	err = execWrapper(ctx, InteractiveExecOptions{
		Run: func(ctx context.Context, options ExecOptions) {
			pctx, cancel := context.WithCancel(context.WithoutCancel(ctx))
			defer cancel()

			pipes, pipesWait, err := e.pipes(pctx, rs, driver, options)
			if err != nil {
				runErr = err
				return
			}
			defer func() {
				if err := pipesWait(); err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("pipe wait: %v", err))
				}
			}()
			defer func() {
				select {
				case <-pctx.Done():
				case <-time.After(200 * time.Millisecond):
					cancel()
				}
			}()

			runRes, runErr = hpanic.RecoverV(func() (*pluginv1.RunResponse, error) {
				return driver.Run(ctx, &pluginsdk.RunRequest{
					RunRequest: pluginv1.RunRequest_builder{
						RequestId:    htypes.Ptr(rs.ID),
						Target:       def.TargetDef.TargetDef,
						SandboxPath:  htypes.Ptr(sandboxfs.Path()),
						TreeRootPath: htypes.Ptr(e.Root.Path()),
						Inputs:       inputs,
						Pipes:        pipes,
						Hashin:       htypes.Ptr(hashin),
					}.Build(),
					Inputs: inputsSdk,
				})
			})
		},
		Pty: def.GetPty(),
	})
	err = errors.Join(err, runErr)
	if err != nil {
		if errors.Is(err, pluginsdk.ErrNotImplemented) {
			return nil, fmt.Errorf("run: %v: %w", def.GetDriver(), err)
		}

		return nil, fmt.Errorf("run: %w", err)
	}

	hashin2, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, fmt.Errorf("hashin2: %w", err)
	}

	if hashin != hashin2 {
		return nil, errors.New("modified during execution")
	}

	if options.shell {
		return &ExecuteResult{
			Def:    def,
			Hashin: hashin,
			AfterCache: func() {
				err := sandboxfs.RemoveAll()
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("failed to remove sandboxfs: %v", err))
				}
			},
		}, nil
	}

	execArtifacts := make([]*ExecuteArtifact, 0, len(def.OutputNames()))

	for _, output := range def.GetOutputs() {
		shouldCollect := false
		for _, path := range output.GetPaths() {
			if path.GetCollect() {
				shouldCollect = true
				break
			}
		}

		if !shouldCollect {
			continue
		}

		tarname := output.GetGroup() + ".tar"
		tarf, err := hfs.Create(sandboxfs.At("collect", tarname))
		if err != nil {
			return nil, err
		}
		defer tarf.Close()

		tar := htar.NewPacker(tarf)
		defer tar.Close()

		for _, path := range output.GetPaths() {
			if !path.GetCollect() {
				continue
			}

			var globPath string
			switch path.WhichContent() {
			case pluginv1.TargetDef_Path_FilePath_case:
				globPath = path.GetFilePath()
			case pluginv1.TargetDef_Path_DirPath_case:
				globPath = path.GetDirPath()
			case pluginv1.TargetDef_Path_Glob_case:
				globPath = path.GetGlob()
			default:
				return nil, fmt.Errorf("unknown path type: %v", path.WhichContent())
			}

			err := hfs.Glob(ctx, cwdfs, globPath, nil, func(entry hfs.GlobEntry) error {
				f, err := hfs.Open(entry.Node)
				if err != nil {
					return err
				}
				defer f.Close()

				err = tar.WriteFile(f, filepath.Join(tref.ToOSPath(def.GetRef().GetPackage()), entry.RelPath))
				if err != nil {
					return err
				}

				return nil
			})
			if err != nil {
				return nil, fmt.Errorf("collect: %v: %w", path, err)
			}
		}

		err = tar.Close()
		if err != nil {
			return nil, err
		}

		err = tarf.Close()
		if err != nil {
			return nil, err
		}

		execArtifact := pluginsdk.ProtoArtifact{
			Artifact: pluginv1.Artifact_builder{
				Group:   htypes.Ptr(output.GetGroup()),
				Name:    htypes.Ptr(tarname),
				Type:    htypes.Ptr(pluginv1.Artifact_TYPE_OUTPUT),
				TarPath: proto.String(tarf.Name()),
			}.Build(),
		}

		hashout, err := e.hashout(ctx, def.GetRef(), execArtifact)
		if err != nil {
			return nil, err
		}

		execArtifacts = append(execArtifacts, &ExecuteArtifact{
			Hashout:  hashout,
			Artifact: execArtifact,
		})
	}

	{
		shouldCollect := false
		for _, path := range def.GetSupportFiles() {
			if path.GetCollect() {
				shouldCollect = true
				break
			}
		}

		if shouldCollect {
			tarname := "support.tar"
			tarf, err := hfs.Create(sandboxfs.At("collect", tarname))
			if err != nil {
				return nil, err
			}
			defer tarf.Close()

			tar := htar.NewPacker(tarf)
			defer tar.Close()

			for _, path := range def.GetSupportFiles() {
				if !path.GetCollect() {
					continue
				}

				var globPath string
				switch path.WhichContent() {
				case pluginv1.TargetDef_Path_FilePath_case:
					globPath = path.GetFilePath()
				case pluginv1.TargetDef_Path_DirPath_case:
					globPath = path.GetDirPath()
				case pluginv1.TargetDef_Path_Glob_case:
					globPath = path.GetGlob()
				default:
					return nil, fmt.Errorf("unknown support file path type: %v", path.WhichContent())
				}

				err := hfs.Glob(ctx, cwdfs, globPath, nil, func(entry hfs.GlobEntry) error {
					f, err := hfs.Open(entry.Node)
					if err != nil {
						return fmt.Errorf("open: %w", err)
					}
					defer f.Close()

					return tar.WriteFile(f, filepath.Join(tref.ToOSPath(def.GetRef().GetPackage()), entry.RelPath))
				})
				if err != nil {
					return nil, fmt.Errorf("collect support file %v: %w", globPath, err)
				}
			}

			err = tar.Close()
			if err != nil {
				return nil, err
			}

			execArtifact := pluginsdk.ProtoArtifact{
				Artifact: pluginv1.Artifact_builder{
					Name:    htypes.Ptr(tarname),
					Type:    htypes.Ptr(pluginv1.Artifact_TYPE_SUPPORT_FILE),
					TarPath: proto.String(tarf.Name()),
				}.Build(),
			}

			hashout, err := e.hashout(ctx, def.GetRef(), execArtifact)
			if err != nil {
				return nil, err
			}

			execArtifacts = append(execArtifacts, &ExecuteArtifact{
				Hashout:  hashout,
				Artifact: execArtifact,
			})
		}
	}

	for _, partifact := range runRes.GetArtifacts() {
		if partifact.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		artifact := pluginsdk.ProtoArtifact{
			Artifact: partifact,
		}

		hashout, err := e.hashout(ctx, def.GetRef(), artifact)
		if err != nil {
			return nil, fmt.Errorf("hashout: %w", err)
		}

		execArtifacts = append(execArtifacts, &ExecuteArtifact{
			Hashout:  hashout,
			Artifact: artifact,
		})
	}

	return ExecuteResult{
		Hashin:    hashin,
		Def:       def,
		Artifacts: execArtifacts,
		AfterCache: func() {
			err := sandboxfs.RemoveAll()
			if err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("failed to remove sandboxfs: %v", err))
			}
		},
	}.Sorted(), nil
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

	if options.storeCache {
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

	cachedArtifacts, err := e.cacheLocally(ctx, def, cacheHashin, artifactsToCache)
	if err != nil {
		return nil, fmt.Errorf("cache locally: %w", err)
	}

	cachedArtifacts = append(cachedArtifacts, artifactsToPassthrough...)

	manifest, err := e.createLocalCacheManifest(ctx, def.GetRef(), hashin, cachedArtifacts)
	if err != nil {
		return nil, fmt.Errorf("create local cache manifest: %w", err)
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
