package engine

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hmaps"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/metric"
	"golang.org/x/sync/semaphore"
	"hash"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"time"

	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"google.golang.org/protobuf/proto"

	"github.com/hephbuild/heph/plugin/tref"

	"connectrpc.com/connect"
	"github.com/dlsniper/debugger"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hpty"
	"github.com/hephbuild/heph/internal/htar"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/heph/plugin/hpipe"
	"github.com/zeebo/xxh3"
	"golang.org/x/sync/errgroup"
)

type ExecOptions struct {
	Stdin  io.Reader
	Stdout io.Writer
	Stderr io.Writer

	interactiveExec func(context.Context, InteractiveExecOptions) error
	shell           bool
	force           bool
}

type InteractiveExecOptions struct {
	Run func(context.Context, ExecOptions)
	Pty bool
}

type ResultOptions struct {
	ExecOptions     ExecOptions
	InteractiveExec func(context.Context, InteractiveExecOptions) error
	Shell           bool
	Force           bool
}

func (e *Engine) Result(ctx context.Context, pkg, name string, outputs []string, options ResultOptions, rc *ResolveCache) (*ExecuteResult, error) {
	res, err := e.ResultFromRef(ctx, &pluginv1.TargetRef{Package: pkg, Name: name}, outputs, options, rc)
	if err != nil {
		return nil, err
	}

	return res.ExecuteResult, nil
}

func (e *Engine) ResultFromRef(ctx context.Context, ref *pluginv1.TargetRef, outputs []string, options ResultOptions, rc *ResolveCache) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromRef", trace.WithAttributes(attribute.String("target", tref.Format(ref))))
	defer span.End()

	return e.result(ctx, DefContainer{Ref: ref}, outputs, options, rc)
}
func (e *Engine) ResultFromDef(ctx context.Context, def *TargetDef, outputs []string, options ResultOptions, rc *ResolveCache) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromDef", trace.WithAttributes(attribute.String("target", tref.Format(def.GetRef()))))
	defer span.End()

	return e.result(ctx, DefContainer{Spec: def.TargetSpec, Def: def.TargetDef}, outputs, options, rc)
}
func (e *Engine) ResultFromSpec(ctx context.Context, spec *pluginv1.TargetSpec, outputs []string, options ResultOptions, rc *ResolveCache) (*ExecuteResultLocks, error) {
	ctx, span := tracer.Start(ctx, "ResultFromSpec", trace.WithAttributes(attribute.String("target", tref.Format(spec.GetRef()))))
	defer span.End()

	return e.result(ctx, DefContainer{Spec: spec}, outputs, options, rc)
}

var meter = otel.Meter("heph_engine")

var resultCounter = sync.OnceValue(func() metric.Int64Counter {
	i, err := meter.Int64Counter("result", metric.WithUnit("{count}"))
	if err != nil {
		panic(err)
	}
	return i
})

func (e *Engine) result(ctx context.Context, c DefContainer, outputs []string, options ResultOptions, rc *ResolveCache) (*ExecuteResultLocks, error) {
	debugger.SetLabels(func() []string {
		return []string{
			fmt.Sprintf("heph/engine: Result %v", tref.Format(c.GetRef())), "",
		}
	})

	def, err, _ := rc.memLink.Do(refKey(c.GetRef()), func() (*LightLinkedTarget, error) {
		return e.LightLink(ctx, c)
	})
	if err != nil {
		return nil, fmt.Errorf("link: %w", err)
	}

	ref := def.GetRef()

	if len(outputs) == 1 && outputs[0] == AllOutputs {
		outputs = def.Outputs
	}

	res, err, computed := rc.memResult.Do(keyRefOutputs(ref, outputs), func() (*ExecuteResultLocks, error) {
		resultCounter().Add(ctx, 1, metric.WithAttributes(
			attribute.String("target", tref.Format(ref)),
		))

		options.ExecOptions.interactiveExec = options.InteractiveExec
		options.ExecOptions.shell = options.Shell
		options.ExecOptions.force = options.Force

		ctx = trace.ContextWithSpan(ctx, e.RootSpan)
		ctx = hstep.WithoutParent(ctx)

		step, ctx := hstep.New(ctx, tref.Format(ref))
		defer step.Done()

		res, err := e.innerResultWithSideEffects(ctx, def, outputs, options, rc)
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

func (e *Engine) depsResults(ctx context.Context, t *LightLinkedTarget, withOutputs bool, rc *ResolveCache) (DepsResults, error) {
	ctx, span := tracer.Start(ctx, "depsResults", trace.WithAttributes(attribute.String("target", tref.Format(t.GetRef()))))
	defer span.End()

	var g errgroup.Group
	results := make(DepsResults, len(t.Inputs))

	for i, dep := range t.Inputs {
		g.Go(func() error {
			outputs := dep.Outputs
			if !withOutputs {
				outputs = nil
			}

			res, err := e.ResultFromDef(ctx, dep.TargetDef, outputs, ResultOptions{}, rc)
			if err != nil {
				return err
			}

			res.Artifacts = slices.DeleteFunc(res.Artifacts, func(output ExecuteResultArtifact) bool {
				return output.GetType() != pluginv1.Artifact_TYPE_OUTPUT
			})

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

	slices.SortFunc(results, func(a, b *ExecuteResultWithOrigin) int {
		if v := tref.Compare(a.Def.GetRef(), b.Def.GetRef()); v != 0 {
			return v
		}

		return strings.Compare(a.Hashin, b.Hashin)
	})

	return results, nil
}

const AllOutputs = "__all_outputs__"

func (e *Engine) resultFromCache(ctx context.Context, def *LightLinkedTarget, outputs []string, rc *ResolveCache, hashin string) (*ExecuteResult, bool, error) {
	res, ok, err := e.ResultFromLocalCache(ctx, def, outputs, hashin)
	if err != nil {
		return nil, false, fmt.Errorf("result from local cache: %w", err)
	}

	if ok {
		step := hstep.From(ctx)
		step.SetText(fmt.Sprintf("%v: cached", tref.Format(def.GetRef())))

		return res, true, nil
	}

	res, ok, err = e.ResultFromRemoteCache(ctx, def, outputs, hashin, rc)
	if err != nil {
		return nil, false, fmt.Errorf("result from remote cache: %w", err)
	}

	if ok {
		return res, true, nil
	}

	return nil, false, nil
}

func (e *Engine) innerResultWithSideEffects(ctx context.Context, def *LightLinkedTarget, outputs []string, options ResultOptions, rc *ResolveCache) (*ExecuteResultLocks, error) {
	res, err := e.innerResult(ctx, def, options, outputs, rc)
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

func (e *Engine) innerResult(ctx context.Context, def *LightLinkedTarget, options ResultOptions, outputs []string, rc *ResolveCache) (_ *ExecuteResultLocks, rerr error) {
	results, err := e.depsResults(ctx, def, false, rc)
	if err != nil {
		return nil, fmt.Errorf("result deps: %w", err)
	}
	unlockDepsResults := sync.OnceFunc(func() {
		results.Unlock(ctx)
	})
	defer unlockDepsResults()

	hashin, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, fmt.Errorf("hashin: %w", err)
	}

	unlockDepsResults()

	getCache := def.Cache && !options.Shell && !options.Force
	storeCache := def.Cache && !options.Shell

	if getCache {
		locks, err := e.lockCache(ctx, def.GetRef(), outputs, hashin, true)
		if err != nil {
			return nil, fmt.Errorf("lock cache: %w", err)
		}

		res, ok, err := e.resultFromCache(ctx, def, outputs, rc, hashin)
		if err != nil {
			return nil, err
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

	res, err, computed := rc.memExecute.Do(refKey(def.GetRef()), func() (*ExecuteResultLocks, error) {
		locks, err := e.lockCache(ctx, def.GetRef(), outputs, hashin, false)
		if err != nil {
			return nil, fmt.Errorf("lock cache: %w", err)
		}

		var res *ExecuteResult
		if storeCache {
			res, err = e.ExecuteAndCache(ctx, def, options.ExecOptions, rc)
			if err != nil {
				err = errors.Join(err, locks.Unlock())

				return nil, err
			}
		} else {
			res, err = e.Execute(ctx, def, options.ExecOptions, rc)
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

	res.Artifacts = slices.DeleteFunc(res.Artifacts, func(artifact ExecuteResultArtifact) bool {
		return !slices.Contains(outputs, artifact.Group)
	})

	return res, nil

}

func (e *Engine) hashin(ctx context.Context, def *LightLinkedTarget, results []*ExecuteResultWithOrigin) (string, error) {
	var h interface {
		hash.Hash
		io.StringWriter
	}
	if false {
		h = newHashWithDebug(xxh3.New(), strings.TrimPrefix(tref.Format(def.GetRef()), "//"))
	} else {
		h = xxh3.New()
	}
	writeProto := func(v proto.Message, ignore map[string]struct{}) error {
		return stableProtoHashEncode(h, v, ignore)
	}

	err := writeProto(def.GetRef(), nil)
	if err != nil {
		return "", err
	}

	ignoreFromHash := e.DriversConfig[def.GetDriver()].GetIgnoreFromHash()

	err = writeProto(def.Def, hmaps.Keyed(ignoreFromHash))
	if err != nil {
		return "", err
	}

	// TODO support fieldmask of deps to include in hashin
	for _, result := range results {
		_, err = h.WriteString(result.InputOrigin.Id)
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

func (e *Engine) ResultFromRemoteCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string, rc *ResolveCache) (*ExecuteResult, bool, error) {
	ctx, span := tracer.Start(ctx, "ResultFromRemoteCache")
	defer span.End()

	// TODO

	return nil, false, nil
}

func (e *Engine) pipes(ctx context.Context, driver pluginv1connect.DriverClient, options ExecOptions) ([]string, func() error, error) {
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

		res, err := driver.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
		if err != nil && connect.CodeOf(err) != connect.CodeUnimplemented {
			return nil, wait, err
		}

		if res != nil && res.Msg.GetId() != "" {
			pipes[0] = res.Msg.GetId()

			ctx, cancel := context.WithCancel(ctx)
			cancels = append(cancels, cancel)

			go func() {
				defer cancel()

				w, err := hpipe.Writer(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.Msg.GetPath())
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
		res, err := driver.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
		if err != nil && connect.CodeOf(err) != connect.CodeUnimplemented {
			return nil, wait, err
		}

		if res != nil && res.Msg.GetId() != "" {
			pipes[1] = res.Msg.GetId()

			eg.Go(func() error {
				r, err := hpipe.Reader(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.Msg.GetPath())
				if err != nil {
					return err
				}

				_, err = io.Copy(options.Stdout, r)

				return err
			})
		}
	}

	if options.Stderr != nil {
		res, err := driver.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
		if err != nil && connect.CodeOf(err) != connect.CodeUnimplemented {
			return nil, wait, err
		}

		if res != nil && res.Msg.GetId() != "" {
			pipes[2] = res.Msg.GetId()

			eg.Go(func() error {
				r, err := hpipe.Reader(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.Msg.GetPath())
				if err != nil {
					return err
				}

				_, err = io.Copy(options.Stderr, r)

				return err
			})
		}
	}

	if stdin, ok := options.Stdin.(*os.File); ok {
		res, err := driver.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
		if err != nil && connect.CodeOf(err) != connect.CodeUnimplemented {
			return nil, wait, err
		}
		if res != nil && res.Msg.GetId() != "" {
			pipes[3] = res.Msg.GetId()

			ch, clean := hpty.WinSizeChan(ctx, stdin)
			cancels = append(cancels, clean)

			go func() {
				w, err := hpipe.Writer(ctx, driverHandle.HTTPClientWithOtel(), driverHandle.GetBaseURL(), res.Msg.GetPath())
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

func (e *Engine) Execute(ctx context.Context, def *LightLinkedTarget, options ExecOptions, rc *ResolveCache) (*ExecuteResult, error) {
	ctx, span := tracer.Start(ctx, "Execute")
	defer span.End()

	debugger.SetLabels(func() []string {
		return []string{
			fmt.Sprintf("heph/engine: Execute %v", tref.Format(def.GetRef())), "",
		}
	})

	results, err := e.depsResults(ctx, def, true, rc)
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

	if def.Cache && !options.force && !options.shell {
		res, ok, err := e.ResultFromLocalCache(ctx, def, def.Outputs, hashin)
		if err != nil {
			return nil, err
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

	if options.interactiveExec == nil {
		options.interactiveExec = func(ctx context.Context, args InteractiveExecOptions) error {
			args.Run(ctx, options)
			return nil
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

	var runRes *connect.Response[pluginv1.RunResponse]
	var runErr error
	err = options.interactiveExec(ctx, InteractiveExecOptions{
		Run: func(ctx context.Context, options ExecOptions) {
			pipes, pipesWait, err := e.pipes(ctx, driver, options)
			if err != nil {
				runErr = err
				return
			}

			runRes, runErr = driver.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
				Target:       def.TargetDef.TargetDef,
				SandboxPath:  sandboxfs.Path(),
				TreeRootPath: e.Root.Path(),
				Inputs:       inputs,
				Pipes:        pipes,
			}))
			if err := pipesWait(); err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("pipe wait: %v", err))
			}
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

		tarf, err = hfs.Open(cachefs, tarname)
		if err != nil {
			return nil, err
		}

		h := xxh3.New()

		_, err = io.Copy(h, tarf)
		if err != nil {
			return nil, err
		}

		hashout := hex.EncodeToString(h.Sum(nil))

		execArtifacts = append(execArtifacts, ExecuteResultArtifact{
			Hashout: hashout,
			Artifact: &pluginv1.Artifact{
				Group:    output.GetGroup(),
				Name:     tarname,
				Type:     pluginv1.Artifact_TYPE_OUTPUT,
				Encoding: pluginv1.Artifact_ENCODING_TAR,
				Uri:      "file://" + tarf.Name(),
			},
		})
	}

	for _, artifact := range runRes.Msg.GetArtifacts() {
		if artifact.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		r, err := hartifact.Reader(ctx, artifact)
		if err != nil {
			return nil, err
		}
		defer r.Close()

		h := xxh3.New()
		_, err = io.Copy(h, r)
		if err != nil {
			return nil, err
		}
		r.Close()

		execArtifacts = append(execArtifacts, ExecuteResultArtifact{
			Hashout:  hex.EncodeToString(h.Sum(nil)),
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

func (e *Engine) ExecuteAndCache(ctx context.Context, def *LightLinkedTarget, options ExecOptions, rc *ResolveCache) (*ExecuteResult, error) {
	res, err := e.Execute(ctx, def, options, rc)
	if err != nil {
		return nil, fmt.Errorf("execute: %w", err)
	}

	var cachedArtifacts []ExecuteResultArtifact
	if res.Executed {
		artifacts, err := e.CacheLocally(ctx, def, res.Hashin, res.Artifacts)
		if err != nil {
			return nil, fmt.Errorf("cache locally: %w", err)
		}

		cachedArtifacts = artifacts
	} else {
		cachedArtifacts = res.Artifacts
	}

	return ExecuteResult{
		Def:       def,
		Hashin:    res.Hashin,
		Artifacts: cachedArtifacts,
	}.Sorted(), nil
}
