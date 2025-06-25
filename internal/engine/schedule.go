package engine

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hinstance"
	engine2 "github.com/hephbuild/heph/lib/engine"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"github.com/hephbuild/heph/tmatch"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/metric"
	"golang.org/x/sync/semaphore"
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

	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"google.golang.org/protobuf/proto"

	"github.com/hephbuild/heph/plugin/tref"

	"github.com/dlsniper/debugger"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hpty"
	"github.com/hephbuild/heph/internal/htar"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/hpipe"
	"github.com/zeebo/xxh3"
	"golang.org/x/sync/errgroup"
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
	hashin      string
}

type InteractiveExecOptions struct {
	Run func(context.Context, ExecOptions)
	Pty bool
}

func (e *Engine) Result(ctx context.Context, pkg, name string, outputs []string, rs *RequestState) (*ExecuteResultLocks, error) {
	res, err := e.ResultFromRef(ctx, &pluginv1.TargetRef{Package: pkg, Name: name}, outputs, rs)
	if err != nil {
		return nil, err
	}

	return res, nil
}

func (e *Engine) ResultFromRef(ctx context.Context, ref *pluginv1.TargetRef, outputs []string, rs *RequestState) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromRef", trace.WithAttributes(attribute.String("target", tref.Format(ref))))
	defer span.End()

	return e.result(ctx, DefContainer{Ref: ref}, outputs, false, rs)
}
func (e *Engine) ResultFromDef(ctx context.Context, def *TargetDef, outputs []string, rs *RequestState) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromDef", trace.WithAttributes(attribute.String("target", tref.Format(def.GetRef()))))
	defer span.End()

	return e.result(ctx, DefContainer{Spec: def.TargetSpec, Def: def.TargetDef}, outputs, false, rs)
}

func (e *Engine) ResultFromSpec(ctx context.Context, spec *pluginv1.TargetSpec, outputs []string, rs *RequestState) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromSpec", trace.WithAttributes(attribute.String("target", tref.Format(spec.GetRef()))))
	defer span.End()

	return e.result(ctx, DefContainer{Spec: spec}, outputs, false, rs)
}

func (e *Engine) ResultsFromMatcher(ctx context.Context, matcher *pluginv1.TargetMatcher, rs *RequestState) ([]*ExecuteResultLocks, error) {
	var out []*ExecuteResultLocks
	var outm sync.Mutex

	var matched bool
	var g errgroup.Group
	for ref, err := range e.Query(ctx, matcher, rs) {
		if err != nil {
			for _, locks := range out {
				locks.Unlock(ctx)
			}

			return nil, err
		}

		matched = true

		g.Go(func() error {
			res, err := e.ResultFromRef(ctx, ref, []string{AllOutputs}, rs)
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
		for _, locks := range out {
			locks.Unlock(ctx)
		}

		return nil, err
	}

	return out, nil
}

type Meta struct {
	Hashin string
}

func (e *Engine) MetaFromDef(ctx context.Context, def *TargetDef, rs *RequestState) (*Meta, error) {
	ctx, span := tracer.Start(ctx, "MetaFromDef", trace.WithAttributes(attribute.String("target", tref.Format(def.GetRef()))))
	defer span.End()

	return e.meta(ctx, DefContainer{Spec: def.TargetSpec, Def: def.TargetDef}, rs)
}

func (e *Engine) meta(ctx context.Context, c DefContainer, rs *RequestState) (*Meta, error) {
	def, err := e.Link(ctx, c, rs)
	if err != nil {
		return nil, fmt.Errorf("link: %w", err)
	}

	manifests, err := e.depsResultMetas(ctx, def, rs)
	if err != nil {
		return nil, fmt.Errorf("deps manifests: %w", err)
	}

	hashin, err := e.hashin2(ctx, def, manifests)
	if err != nil {
		return nil, fmt.Errorf("hashin: %w", err)
	}

	return &Meta{
		Hashin: hashin,
	}, nil
}

var meter = otel.Meter("heph_engine")

var resultCounter = sync.OnceValue(func() metric.Int64Counter {
	i, err := meter.Int64Counter("result", metric.WithUnit("{count}"))
	if err != nil {
		panic(err)
	}
	return i
})

func (e *Engine) result(ctx context.Context, c DefContainer, outputs []string, onlyManifest bool, rs *RequestState) (*ExecuteResultLocks, error) {
	debugger.SetLabels(func() []string {
		return []string{
			fmt.Sprintf("heph/engine: Result %v", tref.Format(c.GetRef())), "",
		}
	})

	def, err := e.Link(ctx, c, rs)
	if err != nil {
		return nil, fmt.Errorf("link: %w", err)
	}

	ref := def.GetRef()

	if def.Cache {
		outputs = def.Outputs
	} else if onlyManifest {
		outputs = nil
	} else if len(outputs) == 1 && outputs[0] == AllOutputs {
		outputs = def.Outputs
	}

	res, err, computed := rs.memResult.Do(keyRefOutputs(ref, outputs), func() (*ExecuteResultLocks, error) {
		resultCounter().Add(ctx, 1, metric.WithAttributes(
			attribute.String("target", tref.Format(ref)),
		))

		ctx = trace.ContextWithSpan(ctx, e.RootSpan)
		ctx = hstep.WithoutParent(ctx)

		step, ctx := hstep.New(ctx, tref.Format(ref))
		defer step.Done()

		res, err := e.innerResultWithSideEffects(ctx, def, outputs, rs)
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

		//e.ResultFromLocalCache(ctx, def, outputs, res.Hashin)

		// TODO: recheck if things are in cache, and if not, clear memResult and call it again
	} else {
		res = res.CloneWithoutLocks()
	}

	if onlyManifest {
		res.Artifacts = slices.DeleteFunc(res.Artifacts, func(artifact ExecuteResultArtifact) bool {
			return artifact.GetType() != pluginv1.Artifact_TYPE_MANIFEST_V1
		})
	} else {
		res.Artifacts = slices.DeleteFunc(res.Artifacts, func(artifact ExecuteResultArtifact) bool {
			return artifact.GetType() == pluginv1.Artifact_TYPE_MANIFEST_V1
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

func (e *Engine) depsResults(ctx context.Context, t *LightLinkedTarget, rs *RequestState) (DepsResults, error) {
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

		return strings.Compare(a.Origin.Id, b.Origin.Id)
	})

	var g errgroup.Group
	results := make(DepsResults, len(inputs))

	for i, dep := range inputs {
		g.Go(func() error {
			res, err := e.ResultFromDef(ctx, dep.TargetDef, dep.Outputs, rs)
			if err != nil {
				return err
			}

			res.Artifacts = slices.DeleteFunc(res.Artifacts, func(output ExecuteResultArtifact) bool {
				return output.GetType() != pluginv1.Artifact_TYPE_OUTPUT
			})

			for _, artifact := range res.Artifacts {
				if artifact.Hashout == "" {
					return fmt.Errorf("%v: output %q has empty hashout", tref.Format(dep.GetRef()), artifact.Group)
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
}

type ResultMetaArtifact struct {
	Hashout string
	Group   string
}

func (e *Engine) ResultMetaFromDef(ctx context.Context, def *TargetDef, outputs []string, rs *RequestState) (ResultMeta, error) {
	res, err := e.result(ctx, DefContainer{Spec: def.TargetSpec, Def: def.TargetDef}, nil, true, rs)
	if err != nil {
		return ResultMeta{}, fmt.Errorf("result: %w", err)
	}
	defer res.Unlock(ctx)

	manifestArtifact, ok := res.FindManifest()
	if !ok {
		return ResultMeta{}, fmt.Errorf("no manifest")
	}

	manifest, err := hartifact.ManifestFromArtifact(ctx, manifestArtifact.Artifact)
	if err != nil {
		return ResultMeta{}, fmt.Errorf("manifest from artifact: %w", err)
	}

	m := ResultMeta{
		Hashin: manifest.Hashin,
	}

	if len(outputs) == 1 && outputs[0] == AllOutputs {
		outputs = def.Outputs
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

	return m, nil
}

func (e *Engine) depsResultMetas(ctx context.Context, t *LightLinkedTarget, rs *RequestState) ([]DepMeta, error) {
	ctx, span := tracer.Start(ctx, "depsManifests", trace.WithAttributes(attribute.String("target", tref.Format(t.GetRef()))))
	defer span.End()

	if len(t.Inputs) == 0 {
		return nil, nil
	}

	inputs := slices.Clone(t.Inputs)
	slices.SortFunc(inputs, func(a, b *LightLinkedTargetInput) int {
		if v := tref.Compare(a.GetRef(), b.GetRef()); v != 0 {
			return v
		}

		return strings.Compare(a.Origin.Id, b.Origin.Id)
	})

	var g errgroup.Group
	results := make([]DepMeta, len(inputs))

	for i, dep := range inputs {
		g.Go(func() error {
			res, err := e.ResultMetaFromDef(ctx, dep.TargetDef, dep.Outputs, rs)
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

func (e *Engine) resultFromCache(ctx context.Context, def *LightLinkedTarget, outputs []string, rs *RequestState, hashin string) (*ExecuteResult, bool, error) {
	res, ok, err := e.ResultFromLocalCache(ctx, def, outputs, hashin)
	if err != nil {
		return nil, false, fmt.Errorf("result from local cache: %w", err)
	}

	if ok {
		return res, true, nil
	}

	res, ok, err = e.ResultFromRemoteCache(ctx, def, outputs, hashin, rs)
	if err != nil {
		return nil, false, fmt.Errorf("result from remote cache: %w", err)
	}

	if ok {
		return res, true, nil
	}

	return nil, false, nil
}

func (e *Engine) innerResultWithSideEffects(ctx context.Context, def *LightLinkedTarget, outputs []string, rs *RequestState) (*ExecuteResultLocks, error) {
	res, err := e.innerResult(ctx, def, outputs, rs)
	if err != nil {
		return nil, err
	}

	// TODO: move after execute
	err = e.codegenTree(ctx, res.Def, res.Artifacts)
	if err != nil {
		return nil, fmt.Errorf("codegen tree: %w", err)
	}

	return res, nil
}

func (e *Engine) innerResult(ctx context.Context, def *LightLinkedTarget, outputs []string, rs *RequestState) (_ *ExecuteResultLocks, rerr error) {
	meta, err := e.MetaFromDef(ctx, def.TargetDef, rs)
	if err != nil {
		return nil, fmt.Errorf("meta: %w", err)
	}

	hashin := meta.Hashin

	shouldShell := tref.Equal(rs.Shell, def.GetRef())
	shouldForce := tmatch.MatchDef(def.TargetSpec, def.TargetDef.TargetDef, rs.Force) == tmatch.MatchYes
	getCache := def.Cache && !shouldShell && !shouldForce
	storeCache := def.Cache && !shouldShell

	if getCache {
		locks, err := e.lockCache(ctx, def.GetRef(), outputs, hashin, true)
		if err != nil {
			return nil, fmt.Errorf("lock cache: %w", err)
		}

		res, ok, err := e.resultFromCache(ctx, def, outputs, rs, hashin)
		if err != nil {
			hlog.From(ctx).With(slog.String("target", tref.Format(def.GetRef())), slog.String("err", err.Error())).Warn("failed to get result from local cache")
		}

		if ok {
			return &ExecuteResultLocks{
				ExecuteResult: res,
				Locks:         locks,
			}, nil
		}

		err = locks.Unlock()
		if err != nil {
			hlog.From(ctx).Error(fmt.Sprintf("%v: failed to unlock result locks: %v", tref.Format(def.GetRef()), err))
		}
	}

	res, err, computed := rs.memExecute.Do(refKey(def.GetRef()), func() (*ExecuteResultLocks, error) {
		if def.TargetSpec.GetDriver() == plugingroup.Name {
			results, err := e.depsResults(ctx, def, rs)
			if err != nil {
				return nil, fmt.Errorf("deps results: %w", err)
			}
			defer results.Unlock(ctx)

			manifest := hartifact.Manifest{
				Version:   "v1",
				Target:    tref.Format(def.GetRef()),
				CreatedAt: time.Now(),
				Hashin:    "", // TODO
			}

			var artifacts []ExecuteResultArtifact
			var locks CacheLocks
			for _, result := range results {
				for _, artifact := range result.Artifacts {
					gartifact := &pluginv1.Artifact{
						Name:    artifact.Name,
						Type:    artifact.GetType(),
						Content: artifact.GetContent(),
					}

					artifacts = append(artifacts, ExecuteResultArtifact{
						Hashout:  artifact.Hashout,
						Artifact: gartifact,
					})

					martifact, err := hartifact.ProtoArtifactToManifest(artifact.Hashout, artifact.Artifact)
					if err != nil {
						return nil, fmt.Errorf("proto artifact to manifest: %w", err)
					}

					manifest.Artifacts = append(manifest.Artifacts, martifact)

					locks.AddFrom(result.Locks)
				}
			}

			martifact, err := hartifact.NewManifestArtifact(manifest)
			if err != nil {
				return nil, fmt.Errorf("new manifest artifact: %w", err)
			}

			artifacts = append(artifacts, ExecuteResultArtifact{
				Artifact: martifact,
			})

			err = locks.RLock(ctx)
			if err != nil {
				return nil, err
			}

			return &ExecuteResultLocks{
				ExecuteResult: ExecuteResult{
					Def:       def,
					Executed:  true,
					Hashin:    hashin,
					Artifacts: artifacts,
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
			hashin:      hashin,
		}

		var res *ExecuteResult
		if storeCache {
			res, err = e.ExecuteAndCache(ctx, def, execOptions, rs)
			if err != nil {
				err = errors.Join(err, locks.Unlock())

				return nil, err
			}
		} else {
			res, err = e.Execute(ctx, def, execOptions, rs)
			if err != nil {
				err = errors.Join(err, locks.Unlock())

				return nil, err
			}
		}

		err = locks.Lock2RLock(ctx)
		if err != nil {
			err = errors.Join(err, locks.Unlock())

			return nil, err
		}

		return &ExecuteResultLocks{
			ExecuteResult: res,
			Locks:         locks,
		}, nil
	})
	if err != nil {
		return nil, err
	}

	if !computed {
		res = res.Clone()

		err := res.Locks.RLock(ctx)
		if err != nil {
			return nil, err
		}
	} else {
		res = res.CloneWithoutLocks()
	}

	return res, nil
}

var enableHashDebug = sync.OnceValue(func() bool {
	v, _ := strconv.ParseBool(os.Getenv("HEPH_HASH_DEBUG"))

	return v
})

func (e *Engine) hashin2(ctx context.Context, def *LightLinkedTarget, results []DepMeta) (string, error) {
	var h interface {
		hash.Hash
		io.StringWriter
	}
	if enableHashDebug() {
		h = newHashWithDebug(xxh3.New(), strings.TrimPrefix(tref.Format(def.GetRef()), "//"))
	} else {
		h = xxh3.New()
	}
	writeProto := func(v proto.Message) error {
		return stableProtoHashEncode(h, v)
	}

	err := writeProto(def.GetRef())
	if err != nil {
		return "", err
	}

	if len(def.Hash) == 0 {
		return "", fmt.Errorf("hash is empty")
	}

	_, err = h.Write(def.Hash)
	if err != nil {
		return "", err
	}

	// TODO support fieldmask of deps to include in hashin
	for _, result := range results {
		_, err = h.WriteString(result.Origin.Id)
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

	if !def.Cache {
		_, err = h.WriteString(hinstance.UID)
		if err != nil {
			return "", err
		}
	}

	hashin := hex.EncodeToString(h.Sum(nil))

	return hashin, nil
}

func (e *Engine) hashin(ctx context.Context, def *LightLinkedTarget, results []*ExecuteResultWithOrigin) (string, error) {
	var metas []DepMeta

	for _, result := range results {
		var artifacts []DepMetaArtifact
		for _, artifact := range result.Artifacts {
			artifacts = append(artifacts, DepMetaArtifact{
				Hashout: artifact.Hashout,
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

	return e.hashin2(ctx, def, metas)
}

type ExecuteResultArtifact struct {
	Hashout string
	*pluginv1.Artifact
}

type ExecuteResult struct {
	Def       *LightLinkedTarget
	Executed  bool
	Hashin    string
	Artifacts []ExecuteResultArtifact
}

func (r ExecuteResult) FindManifest() (ExecuteResultArtifact, bool) {
	for _, artifact := range r.Artifacts {
		if artifact.GetType() != pluginv1.Artifact_TYPE_MANIFEST_V1 {
			continue
		}

		return artifact, true
	}

	return ExecuteResultArtifact{}, false
}

func (r ExecuteResult) FindOutputs(group string) []ExecuteResultArtifact {
	res := make([]ExecuteResultArtifact, 0, len(r.Artifacts))
	for _, artifact := range r.Artifacts {
		if artifact.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}
		if artifact.Group != group {
			continue
		}

		res = append(res, artifact)
	}

	return res
}

type ExecuteResultLocks struct {
	*ExecuteResult
	Locks *CacheLocks
}

func (r *ExecuteResultLocks) Clone() *ExecuteResultLocks {
	return &ExecuteResultLocks{
		ExecuteResult: r.ExecuteResult.Clone(),
		Locks:         r.Locks.Clone(),
	}
}

func (r *ExecuteResultLocks) CloneWithoutLocks() *ExecuteResultLocks {
	return &ExecuteResultLocks{
		ExecuteResult: r.ExecuteResult.Clone(),
		Locks:         r.Locks,
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
	slices.SortFunc(r.Artifacts, func(a, b ExecuteResultArtifact) int {
		return strings.Compare(a.Hashout, b.Hashout)
	})

	return &r
}

func (r ExecuteResult) Clone() *ExecuteResult {
	return &ExecuteResult{
		Def:       r.Def.Clone(),
		Executed:  r.Executed,
		Hashin:    r.Hashin,
		Artifacts: slices.Clone(r.Artifacts),
	}
}

type ExecuteResultWithOrigin struct {
	*ExecuteResultLocks
	InputOrigin *pluginv1.TargetDef_InputOrigin
}

func (e *Engine) pipes(ctx context.Context, driver engine2.Driver, options ExecOptions, rs *RequestState) ([]string, func() error, error) {
	pipes := []string{"", "", "", "", ""}
	eg := &errgroup.Group{}

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

		return err
	}

	driverHandle := e.DriversHandle[driver]

	if options.Stdin != nil {
		stdinErrCh = make(chan error)

		res, err := driver.Pipe(ctx, &pluginv1.PipeRequest{
			RequestId: rs.ID,
		})
		if err != nil && errors.Is(err, engine2.ErrNotImplemented) {
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
		res, err := driver.Pipe(ctx, &pluginv1.PipeRequest{
			RequestId: rs.ID,
		})
		if err != nil && errors.Is(err, engine2.ErrNotImplemented) {
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
		res, err := driver.Pipe(ctx, &pluginv1.PipeRequest{
			RequestId: rs.ID,
		})
		if err != nil && errors.Is(err, engine2.ErrNotImplemented) {
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
		res, err := driver.Pipe(ctx, &pluginv1.PipeRequest{
			RequestId: rs.ID,
		})
		if err != nil && errors.Is(err, engine2.ErrNotImplemented) {
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

var sem = semaphore.NewWeighted(1000 * int64(runtime.GOMAXPROCS(-1)))

func (e *Engine) Execute(ctx context.Context, def *LightLinkedTarget, options ExecuteOptions, rs *RequestState) (*ExecuteResult, error) {
	ctx, span := tracer.Start(ctx, "Execute")
	defer span.End()

	debugger.SetLabels(func() []string {
		return []string{
			fmt.Sprintf("heph/engine: Execute %v", tref.Format(def.GetRef())), "",
		}
	})

	results, err := e.depsResults(ctx, def, rs)
	if err != nil {
		return nil, fmt.Errorf("deps results: %w", err)
	}
	defer results.Unlock(ctx)

	driver, ok := e.DriversByName[def.TargetSpec.GetDriver()]
	if !ok {
		return nil, fmt.Errorf("driver not found: %v", def.TargetSpec.GetDriver())
	}

	if options.shell {
		shellDriver := def.TargetSpec.GetDriver() + "@shell"
		shellDriver = "bash@shell" // TODO: make the original driver declare the shell config
		driver, ok = e.DriversByName[shellDriver]
		if !ok {
			return nil, fmt.Errorf("shell driver not found: %v", shellDriver)
		}
		def.Pty = true
	}

	var targetfolder string
	if def.Cache || options.shell {
		targetfolder = e.targetDirName(def.GetRef())
	} else {
		targetfolder = fmt.Sprintf("__%v__%v", e.targetDirName(def.GetRef()), time.Now().UnixNano())
	}

	hashin, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, fmt.Errorf("hashin1: %w", err)
	}

	if hashin != options.hashin {
		return nil, fmt.Errorf("results hashin (%v) != meta hashin (%v)", hashin, options.hashin)
	}

	if def.Cache && !options.force && !options.shell {
		res, ok, err := e.ResultFromLocalCache(ctx, def, def.Outputs, hashin)
		if err != nil {
			hlog.From(ctx).With(slog.String("target", tref.Format(def.GetRef())), slog.String("err", err.Error())).Warn("failed to get result from local cache")
		}

		if ok {
			step := hstep.From(ctx)
			step.SetText(fmt.Sprintf("%v: cached", tref.Format(def.GetRef())))

			return res, nil
		}
	}

	err = sem.Acquire(ctx, 1)
	if err != nil {
		return nil, err
	}
	defer sem.Release(1)

	step, ctx := hstep.New(ctx, "Running...")
	defer step.Done()

	sandboxfs := hfs.At(e.Sandbox, def.GetRef().GetPackage(), targetfolder)
	workdirfs := hfs.At(sandboxfs, "ws") // TODO: remove the ws from here
	cwdfs := hfs.At(workdirfs, def.GetRef().GetPackage())

	err = sandboxfs.RemoveAll("")
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

	var inputs []*pluginv1.ArtifactWithOrigin
	for _, result := range results {
		for _, artifact := range result.Artifacts {
			inputs = append(inputs, &pluginv1.ArtifactWithOrigin{
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

			pipes, pipesWait, err := e.pipes(pctx, driver, options, rs)
			if err != nil {
				runErr = err
				return
			}
			defer func() {
				if err := pipesWait(); err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("pipe wait: %v", err))
				}
			}()

			runRes, runErr = driver.Run(ctx, &pluginv1.RunRequest{
				RequestId:    rs.ID,
				Target:       def.TargetDef.TargetDef,
				SandboxPath:  sandboxfs.Path(),
				TreeRootPath: e.Root.Path(),
				Inputs:       inputs,
				Pipes:        pipes,
			})
		},
		Pty: def.Pty,
	})
	err = errors.Join(err, runErr)
	if err != nil {
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
			Def:      def,
			Hashin:   hashin,
			Executed: true,
		}, nil
	}

	cachefs := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)
	execArtifacts := make([]ExecuteResultArtifact, 0, len(def.CollectOutputs))

	for _, output := range def.CollectOutputs {
		tarname := output.GetGroup() + ".tar"
		tarf, err := hfs.Create(cachefs, tarname)
		if err != nil {
			return nil, err
		}
		defer tarf.Close()

		tar := htar.NewPacker(tarf)
		for _, path := range output.GetPaths() {
			found := false
			err := hfs.Glob(ctx, cwdfs, path, nil, func(path string, d hfs.DirEntry) error {
				f, err := hfs.Open(cwdfs, path)
				if err != nil {
					return err
				}
				defer f.Close()

				found = true

				err = tar.WriteFile(f, filepath.Join(def.GetRef().GetPackage(), path))
				if err != nil {
					return err
				}

				return nil
			})
			if err != nil {
				return nil, fmt.Errorf("collect: %v: %w", path, err)
			}

			if !found {
				return nil, fmt.Errorf("collect: %v: not found", path)
			}
		}

		err = tarf.Close()
		if err != nil {
			return nil, err
		}

		execArtifacts = append(execArtifacts, ExecuteResultArtifact{
			Artifact: &pluginv1.Artifact{
				Group: output.GetGroup(),
				Name:  tarname,
				Type:  pluginv1.Artifact_TYPE_OUTPUT,
				Content: &pluginv1.Artifact_TarPath{
					TarPath: tarf.Name(),
				},
			},
		})
	}

	for _, artifact := range runRes.GetArtifacts() {
		if artifact.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		execArtifacts = append(execArtifacts, ExecuteResultArtifact{
			Artifact: artifact,
		})

		// panic("copy to cache not implemented yet")

		// TODO: copy to cache
		// hfs.Copy()
		//
		// artifact.Uri
		//
		// execOutputs = append(execOutputs, ExecuteResultOutput{
		//	Name:    artifact.Group,
		//	Hashout: "",
		//	TarPath: "",
		// })
	}

	err = sandboxfs.RemoveAll("")
	if err != nil {
		return nil, err
	}

	return ExecuteResult{
		Hashin:    hashin,
		Def:       def,
		Executed:  true,
		Artifacts: execArtifacts,
	}.Sorted(), nil
}

func (e *Engine) ExecuteAndCache(ctx context.Context, def *LightLinkedTarget, options ExecuteOptions, rs *RequestState) (*ExecuteResult, error) {
	res, err := e.Execute(ctx, def, options, rs)
	if err != nil {
		return nil, fmt.Errorf("execute: %w", err)
	}

	var cachedArtifacts []ExecuteResultArtifact
	if res.Executed {
		artifacts, manifest, err := e.CacheLocally(ctx, def, res.Hashin, res.Artifacts)
		if err != nil {
			return nil, fmt.Errorf("cache locally: %w", err)
		}

		cachedArtifacts = artifacts

		// TODO: move this to a background execution model , so that local build can proceed, while this is uploading in the background
		e.CacheRemotely(ctx, def, res.Hashin, manifest, cachedArtifacts)
	} else {
		cachedArtifacts = res.Artifacts
	}

	return ExecuteResult{
		Def:       def,
		Hashin:    res.Hashin,
		Artifacts: cachedArtifacts,
	}.Sorted(), nil
}
