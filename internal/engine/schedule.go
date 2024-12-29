package engine

import (
	"connectrpc.com/connect"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/dlsniper/debugger"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/hinstance"
	"github.com/hephbuild/hephv2/internal/hlocks"
	"github.com/hephbuild/hephv2/internal/hpty"
	"github.com/hephbuild/hephv2/internal/htar"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/hephv2/plugin/hpipe"
	"github.com/zeebo/xxh3"
	"golang.org/x/sync/errgroup"
	"google.golang.org/protobuf/proto"
	"io"
	"os"
	"slices"
	"sync"
	"time"
)

type ExecOptions struct {
	Stdin  io.Reader
	Stdout io.Writer
	Stderr io.Writer

	interactiveExec func(InteractiveExecOptions) error
	shell           bool
}

type InteractiveExecOptions struct {
	Run func(ExecOptions)
	Pty bool
}

type ResultOptions struct {
	ExecOptions     ExecOptions
	InteractiveExec func(InteractiveExecOptions) error
	Shell           bool

	Singleflight *Singleflight
}

func (e *Engine) Result(ctx context.Context, pkg, name string, outputs []string, options ResultOptions) <-chan *ExecuteResult {
	if options.Singleflight == nil {
		options.Singleflight = &Singleflight{}
	}

	options.ExecOptions.interactiveExec = options.InteractiveExec
	options.ExecOptions.shell = options.Shell

	ctx = hstep.WithoutParent(ctx)

	ch, send, isNew := options.Singleflight.Result(ctx, pkg, name, outputs)
	if !isNew {
		return ch
	}

	go func() {
		step, ctx := hstep.New(ctx, fmt.Sprintf("//%v:%v", pkg, name))
		defer step.Done()

		res, err := e.innerResult(ctx, pkg, name, outputs, options)
		if err != nil {
			step.SetError()
			send(&ExecuteResult{Err: fmt.Errorf("%v:%v %w", pkg, name, err)})
			return
		}

		if res.Err != nil {
			step.SetError()
			res.Err = fmt.Errorf("%v:%v %w", pkg, name, res.Err)
		}

		send(res)
	}()

	return ch
}

func (e *Engine) depsResults(ctx context.Context, def *LightLinkedTarget, withOutputs bool, sf *Singleflight) []*ExecuteResultWithOrigin {
	var wg sync.WaitGroup
	results := make([]*ExecuteResultWithOrigin, len(def.Deps))
	wg.Add(len(def.Deps))

	for i, dep := range def.Deps {
		go func() {
			defer wg.Done()

			outputs := dep.Outputs
			if !withOutputs {
				outputs = nil
			}

			ch := e.Result(ctx, dep.Ref.Package, dep.Ref.Name, outputs, ResultOptions{Singleflight: sf})

			res := <-ch

			res.Outputs = slices.DeleteFunc(res.Outputs, func(output ExecuteResultOutput) bool {
				return output.Type != pluginv1.Artifact_TYPE_OUTPUT
			})

			results[i] = &ExecuteResultWithOrigin{
				ExecuteResult: res,
				Origin:        dep.DefDep,
			}
		}()
	}

	wg.Wait()

	return results
}

const AllOutputs = "__all_outputs__"

func (e *Engine) errFromDepsResults(results []*ExecuteResultWithOrigin, def *LightLinkedTarget) error {
	var errs error
	for i, result := range results {
		if result.Err != nil {
			errs = errors.Join(errs, fmt.Errorf("%v: %w", def.Deps[i].Ref, result.Err))
		}
	}

	return errs
}

func (e *Engine) ResultFromCache(ctx context.Context, def *LightLinkedTarget, outputs []string, sf *Singleflight) (*ExecuteResult, bool, error) {
	// Get hashout from all deps
	results := e.depsResults(ctx, def, false, sf)
	err := e.errFromDepsResults(results, def)
	if err != nil {
		return nil, false, fmt.Errorf("result deps: %w", err)
	}

	hashin, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, false, fmt.Errorf("hashin: %w", err)
	}

	res, ok, err := e.ResultFromLocalCache(ctx, def, outputs, hashin)
	if err != nil {
		return nil, false, fmt.Errorf("result from local cache: %w", err)
	}

	if ok {
		step := hstep.From(ctx)
		step.SetText(fmt.Sprintf("//%v:%v: cached", def.Ref.Package, def.Ref.Name))

		return res, true, nil
	}

	res, ok, err = e.ResultFromRemoteCache(ctx, def, outputs, hashin)
	if err != nil {
		return nil, false, fmt.Errorf("result from remote cache: %w", err)
	}

	if ok {
		return res, true, nil
	}

	return nil, false, nil
}

func (e *Engine) innerResult(ctx context.Context, pkg, name string, outputs []string, options ResultOptions) (*ExecuteResult, error) {
	def, err := e.LightLink(ctx, pkg, name)
	if err != nil {
		return nil, err
	}

	if options.Shell {
		def.Cache = false
	}

	if len(outputs) == 1 && outputs[0] == AllOutputs {
		outputs = def.Outputs
	}

	var targetfolder string
	if def.Cache {
		targetfolder = "__" + def.Ref.Name
	} else {
		targetfolder = fmt.Sprintf("__%v__%v", def.Ref.Name, time.Now().UnixNano())
	}

	l := hlocks.NewFlock(hfs.At(e.Home, "locks", def.Ref.Package, targetfolder), "", "result.lock")

	if def.Cache {
		res, ok, err := e.ResultFromCache(ctx, def, outputs, options.Singleflight)
		if err != nil {
			return nil, err
		}

		if ok {
			return res, nil
		}

		err = l.Lock(ctx)
		if err != nil {
			return nil, err
		}

		defer func() {
			err := l.Unlock()
			if err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("failed unlocking: %v", err))
			}
		}()

		return e.ExecuteAndCache(ctx, def, options.ExecOptions, options.Singleflight)
	} else {
		err = l.Lock(ctx)
		if err != nil {
			return nil, err
		}

		defer func() {
			err := l.Unlock()
			if err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("failed unlocking: %v", err))
			}
		}()

		return e.Execute(ctx, def, options.ExecOptions, options.Singleflight)
	}
}

func (e *Engine) hashin(ctx context.Context, def *LightLinkedTarget, results []*ExecuteResultWithOrigin) (string, error) {
	h := xxh3.New()
	b, err := proto.Marshal(def.Ref)
	if err != nil {
		return "", err
	}
	_, err = h.Write(b)
	if err != nil {
		return "", err
	}

	// TODO support fieldmask of things to include in hashin
	b, err = proto.Marshal(def.Def)
	if err != nil {
		return "", err
	}
	_, err = h.Write(b)
	if err != nil {
		return "", err
	}

	// TODO support fieldmask of deps to include in hashin
	for _, result := range results {
		b, err = proto.Marshal(result.Origin.Ref)
		if err != nil {
			return "", err
		}
		_, err = h.Write(b)
		if err != nil {
			return "", err
		}

		for _, output := range result.Outputs {
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

type ExecuteResultOutput struct {
	Hashout string
	*pluginv1.Artifact
}

type ExecuteResult struct {
	Err      error
	Hashin   string
	Outputs  []ExecuteResultOutput
	Executed bool
}

type ExecuteResultWithOrigin struct {
	*ExecuteResult
	Origin *pluginv1.TargetDef_Dep
}

func (e *Engine) ResultFromRemoteCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
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
		if err != nil {
			return nil, wait, err
		}

		pipes[0] = res.Msg.Id

		ctx, cancel := context.WithCancel(ctx)
		cancels = append(cancels, cancel)

		go func() {
			defer cancel()

			w, err := hpipe.Writer(ctx, driverHandle.HttpClient(), driverHandle.BaseURL(), res.Msg.Path)
			if err != nil {
				stdinErrCh <- err
				return
			}
			defer w.Close()

			_, err = io.Copy(w, options.Stdin)

			stdinErrCh <- err
		}()
	}

	if options.Stdout != nil {
		res, err := driver.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
		if err != nil {
			return nil, wait, err
		}

		pipes[1] = res.Msg.Id

		eg.Go(func() error {
			r, err := hpipe.Reader(ctx, driverHandle.HttpClient(), driverHandle.BaseURL(), res.Msg.Path)
			if err != nil {
				return err
			}

			_, err = io.Copy(options.Stdout, r)

			return err
		})
	}

	if options.Stderr != nil {
		res, err := driver.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
		if err != nil {
			return nil, wait, err
		}

		pipes[2] = res.Msg.Id

		eg.Go(func() error {
			r, err := hpipe.Reader(ctx, driverHandle.HttpClient(), driverHandle.BaseURL(), res.Msg.Path)
			if err != nil {
				return err
			}

			_, err = io.Copy(options.Stderr, r)

			return err
		})
	}

	if stdin, ok := options.Stdin.(*os.File); ok {
		res, err := driver.Pipe(ctx, connect.NewRequest(&pluginv1.PipeRequest{}))
		if err != nil {
			return nil, wait, err
		}

		pipes[3] = res.Msg.Id

		ch, clean := hpty.WinSizeChan(ctx, stdin)
		cancels = append(cancels, clean)

		go func() {
			w, err := hpipe.Writer(ctx, driverHandle.HttpClient(), driverHandle.BaseURL(), res.Msg.Path)
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

	return pipes, wait, nil
}

func (e *Engine) Execute(ctx context.Context, def *LightLinkedTarget, options ExecOptions, sf *Singleflight) (*ExecuteResult, error) {
	debugger.SetLabels(func() []string {
		return []string{
			fmt.Sprintf("hephv2/engine: Execute %v %v", def.Ref.Package, def.Ref.Name), "",
		}
	})

	results := e.depsResults(ctx, def, true, sf)
	err := e.errFromDepsResults(results, def)
	if err != nil {
		return nil, fmt.Errorf("deps results: %w", err)
	}

	driver, ok := e.DriversByName[def.Ref.Driver]
	if !ok {
		return nil, fmt.Errorf("driver not found: %v", def.Ref.Driver)
	}

	if options.shell {
		// TODO: make the original driver declare the shell config
		driver, ok = e.DriversByName[def.Ref.Driver+"@shell"]
		if !ok {
			return nil, fmt.Errorf("shell driver not found: %v", def.Ref.Driver)
		}
		def.Pty = true
	}

	var targetfolder string
	if def.Cache {
		targetfolder = "__" + def.Ref.Name
	} else {
		targetfolder = fmt.Sprintf("__%v__%v", def.Ref.Name, time.Now().UnixNano())
	}

	hashin, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, fmt.Errorf("hashin1: %w", err)
	}

	if def.Cache {
		res, ok, err := e.ResultFromLocalCache(ctx, def, def.Outputs, hashin)
		if err != nil {
			return nil, err
		}

		if ok {
			step := hstep.From(ctx)
			step.SetText(fmt.Sprintf("//%v:%v: cached", def.Ref.Package, def.Ref.Name))

			return res, nil
		}
	}

	step, ctx := hstep.New(ctx, "Running...")
	defer step.Done()

	sandboxfs := hfs.At(e.Sandbox, def.Ref.Package, targetfolder)
	workdirfs := sandboxfs.At("ws") // TODO: remove the ws from here

	err = sandboxfs.RemoveAll("")
	if err != nil {
		return nil, err
	}

	inputArtifacts, err := SetupSandbox(ctx, results, workdirfs)
	if err != nil {
		return nil, fmt.Errorf("setup sandbox: %w", err)
	}

	hashin2, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, fmt.Errorf("hashin2: %w", err)
	}

	if hashin != hashin2 {
		return nil, fmt.Errorf("modified while creating sandbox")
	}

	if options.interactiveExec == nil {
		options.interactiveExec = func(args InteractiveExecOptions) error {
			args.Run(options)
			return nil
		}
	}

	var runRes *connect.Response[pluginv1.RunResponse]
	var runErr error
	err = options.interactiveExec(InteractiveExecOptions{
		Run: func(options ExecOptions) {
			pipes, pipesWait, err := e.pipes(ctx, driver, options)
			if err != nil {
				runErr = err
				return
			}

			runRes, runErr = driver.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
				Target:      def.TargetDef,
				SandboxPath: sandboxfs.Path(),
				Inputs:      inputArtifacts,
				Pipes:       pipes,
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

	if options.shell {
		return &ExecuteResult{
			Hashin:   hashin,
			Executed: true,
		}, nil
	}

	cachefs := hfs.At(e.Cache, def.Ref.Package, "__"+def.Ref.Name, hashin)
	var execOutputs []ExecuteResultOutput

	for _, output := range def.CollectOutputs {
		tarname := output.Group + ".tar"
		tarf, err := hfs.Create(cachefs, tarname)
		if err != nil {
			return nil, err
		}
		defer tarf.Close()

		tar := htar.NewPacker(tarf)
		for _, path := range output.Paths {
			// TODO: glob

			f, err := hfs.Open(workdirfs, path)
			if err != nil {
				return nil, fmt.Errorf("collect: open: %w", err)
			}
			defer f.Close()

			err = tar.WriteFile(f, path)
			if err != nil {
				return nil, err
			}

			err = f.Close()
			if err != nil {
				return nil, err
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

		execOutputs = append(execOutputs, ExecuteResultOutput{
			Hashout: hashout,
			Artifact: &pluginv1.Artifact{
				Group:    output.Group,
				Name:     tarname,
				Type:     pluginv1.Artifact_TYPE_OUTPUT,
				Encoding: pluginv1.Artifact_ENCODING_TAR,
				Uri:      "file://" + tarf.Name(),
			},
		})
	}

	for _, artifact := range runRes.Msg.Artifacts {
		if artifact.Type != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		//panic("copy to cache not implemented yet")

		// TODO: copy to cache
		//hfs.Copy()
		//
		//artifact.Uri
		//
		//execOutputs = append(execOutputs, ExecuteResultOutput{
		//	Name:    artifact.Group,
		//	Hashout: "",
		//	TarPath: "",
		//})
	}

	err = sandboxfs.RemoveAll("")
	if err != nil {
		return nil, err
	}

	return &ExecuteResult{
		Hashin:   hashin,
		Executed: true,
		Outputs:  execOutputs,
	}, nil
}

func (e *Engine) ExecuteAndCache(ctx context.Context, def *LightLinkedTarget, options ExecOptions, sf *Singleflight) (*ExecuteResult, error) {
	res, err := e.Execute(ctx, def, options, sf)
	if err != nil {
		return nil, fmt.Errorf("execute: %v", err)
	}

	var cachedArtifacts []ExecuteResultOutput
	if res.Executed {
		artifacts, err := e.CacheLocally(ctx, def, res.Hashin, res.Outputs)
		if err != nil {
			return nil, fmt.Errorf("cache locally: %v", err)
		}

		cachedArtifacts = artifacts
	} else {
		cachedArtifacts = res.Outputs
	}

	return &ExecuteResult{
		Hashin:  res.Hashin,
		Outputs: cachedArtifacts,
	}, nil
}

/*
0. get the hashout from deps
	-> recursive call on transitive deps
1. hash the deps => hashin
2. check if hashin has data present for the requested outputs:
	a. yes
		-> return that
	b. no
		-> go to 3.
3. check if hashin has data present in any cache
	a. yes
		-> attempt to pull them all
			i. success
				-> return that
			ii. failure
				-> go to 4.
	b. no
		-> go to 4.
4. get the result from deps
	-> recursive call to routine
5. execute
*/
