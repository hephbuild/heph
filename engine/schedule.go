package engine

import (
	"connectrpc.com/connect"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/hephv2/hfs"
	"github.com/hephbuild/hephv2/hlocks"
	"github.com/hephbuild/hephv2/htar"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	"google.golang.org/protobuf/proto"
	"io"
	"os"
	"sync"
	"time"
)

func ExecuteResultErr(err error) chan *ExecuteResult {
	ch := make(chan *ExecuteResult)
	go func() {
		ch <- &ExecuteResult{Err: err}
		close(ch)
	}()
	return ch
}

func (e *Engine) Result(ctx context.Context, pkg, name string, outputs []string) chan *ExecuteResult {
	ch := make(chan *ExecuteResult)
	go func() {
		defer close(ch)
		res, err := e.innerResult(ctx, pkg, name, outputs)
		if err != nil {
			ch <- &ExecuteResult{Err: err}
			return
		}

		ch <- res
	}()

	return ch
}

func (e *Engine) depsResults(ctx context.Context, def *LightLinkedTarget, withOutputs bool) []*ExecuteResult {
	var wg sync.WaitGroup
	results := make([]*ExecuteResult, len(def.Deps))
	wg.Add(len(def.Deps))

	for i, dep := range def.Deps {
		go func() {
			defer wg.Done()

			outputs := dep.Outputs
			if !withOutputs {
				outputs = nil
			}

			ch := e.Result(ctx, dep.Ref.Package, dep.Ref.Name, outputs)

			res := <-ch
			results[i] = res
		}()
	}

	wg.Wait()

	return results
}

func (e *Engine) errFromDepsResults(results []*ExecuteResult, def *LightLinkedTarget) error {
	var errs error
	for i, result := range results {
		if result.Err != nil {
			errs = errors.Join(errs, fmt.Errorf("%v: %w", def.Deps[i].Ref, result.Err))
		}
	}

	return errs
}

func (e *Engine) innerResult(ctx context.Context, pkg, name string, outputs []string) (*ExecuteResult, error) {
	def, err := e.LightLink(ctx, pkg, name)
	if err != nil {
		return nil, err
	}

	if def.Cache {
		// Get hashout from all deps
		results := e.depsResults(ctx, def, false)
		err = e.errFromDepsResults(results, def)
		if err != nil {
			return nil, err
		}

		hashin, err := e.hashin(ctx, def, results)
		if err != nil {
			return nil, err
		}

		res, ok, err := e.ResultFromLocalCache(ctx, def, outputs, hashin)
		if err != nil {
			return nil, err
		}

		if ok {
			return res, nil
		}

		res, ok, err = e.ResultFromRemoteCache(ctx, def, outputs, hashin)
		if err != nil {
			return nil, err
		}

		if ok {
			return res, nil
		}

		return e.ExecuteAndCache(ctx, def)
	} else {
		return e.Execute(ctx, def)
	}
}

func (e *Engine) hashin(ctx context.Context, def *LightLinkedTarget, results []*ExecuteResult) (string, error) {
	h := xxh3.New()
	b, err := proto.Marshal(def.Ref)
	if err != nil {
		_, err = h.Write(b)
		if err != nil {
			return "", err
		}
	}
	// TODO support fieldmask of things to include in hashin
	b, err = proto.Marshal(def.Def)
	if err != nil {
		_, err = h.Write(b)
		if err != nil {
			return "", err
		}
	}
	// TODO support fieldmask of deps to include in hashin
	for _, result := range results {
		for _, output := range result.Outputs {
			_, err = h.WriteString(output.Hashout)
			if err != nil {
				return "", err
			}
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
	Err error

	Hashin string

	Outputs []ExecuteResultOutput
}

func (e *Engine) ResultFromLocalCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
	multi := hlocks.NewMulti()

	res, ok, err := e.resultFromLocalCacheInner(ctx, def, outputs, hashin, multi)
	if err != nil {
		if err := multi.UnlockAll(); err != nil {
			// TODO: log
		}

		// if the file doesnt exist, thats not an error, just means the cache doesnt exist locally
		if errors.Is(err, os.ErrNotExist) {
			return nil, false, nil
		}

		return nil, false, err
	}

	return res, ok, nil
}

type ManifestArtifact struct {
	Hashout string

	Group    string
	Name     string
	Type     pluginv1.Artifact_Type
	Encoding pluginv1.Artifact_Encoding
	Path     string
}

type Manifest struct {
	Version   string
	Artifacts []ManifestArtifact
}

func (m Manifest) GetArtifacts(output string) []ManifestArtifact {
	a := make([]ManifestArtifact, 0)
	for _, artifact := range m.Artifacts {
		if artifact.Group != output {
			continue
		}

		a = append(a, artifact)
	}

	return a
}

func (e *Engine) resultFromLocalCacheInner(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string, locks *hlocks.Multi) (*ExecuteResult, bool, error) {
	dirfs := hfs.At(e.Cache, def.Ref.Package, "__"+def.Ref.Name, hashin)

	{
		l := hlocks.NewFlock2(dirfs, "", "manifest.json", false)
		err := l.RLock(ctx)
		if err != nil {
			return nil, false, err
		}
		locks.Add(l.RUnlock)
	}

	mainfestb, err := hfs.ReadFile(dirfs, "manifest.json")
	if err != nil {
		return nil, false, err
	}

	var manifest Manifest
	err = json.Unmarshal(mainfestb, &manifest)
	if err != nil {
		return nil, false, err
	}

	var artifacts []ManifestArtifact
	for _, output := range outputs {
		outputArtifacts := manifest.GetArtifacts(output)

		artifacts = append(artifacts, outputArtifacts...)
	}

	for _, artifact := range artifacts {
		l := hlocks.NewFlock2(dirfs, "", artifact.Path, false)
		err := l.RLock(ctx)
		if err != nil {
			return nil, false, err
		}
		locks.Add(l.RUnlock)
	}

	{
		l := hlocks.NewFlock2(dirfs, "", "hashin", false)
		err := l.RLock(ctx)
		if err != nil {
			return nil, false, err
		}
		locks.Add(l.RUnlock)
	}

	execOutputs := make([]ExecuteResultOutput, len(artifacts))
	for _, artifact := range artifacts {
		execOutputs = append(execOutputs, ExecuteResultOutput{
			Hashout: artifact.Hashout,
			Artifact: &pluginv1.Artifact{
				Group:    artifact.Group,
				Name:     artifact.Name,
				Type:     pluginv1.Artifact_TYPE_OUTPUT,
				Encoding: artifact.Encoding,
				Uri:      "file://" + dirfs.Path(artifact.Path),
			},
		})
	}

	hashinb, err := hfs.ReadFile(dirfs, "hashin")
	if err != nil {
		return nil, false, err
	}

	return &ExecuteResult{
		Hashin:  string(hashinb),
		Outputs: execOutputs,
	}, true, nil
}

func (e *Engine) ResultFromRemoteCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
	// TODO

	return nil, false, nil
}

func (e *Engine) Execute(ctx context.Context, def *LightLinkedTarget) (*ExecuteResult, error) {
	results := e.depsResults(ctx, def, true)
	err := e.errFromDepsResults(results, def)
	if err != nil {
		return nil, err
	}

	driver, ok := e.DriversByName[def.Ref.Driver]
	if !ok {
		return nil, fmt.Errorf("driver not found: %v", def.Ref.Driver)
	}

	var targetfolder string
	if def.Cache {
		targetfolder = "__" + def.Ref.Name
	} else {
		targetfolder = fmt.Sprintf("__%v__%v", def.Ref.Name, time.Now().UnixNano())
	}

	sandboxfs := hfs.At(e.Sandbox, def.Ref.Package, targetfolder)

	l := hlocks.NewFlock(hfs.At(e.Home, "locks", def.Ref.Package, targetfolder), "", "exec.lock")

	err = l.Lock(ctx)
	if err != nil {
		return nil, err
	}
	defer func() {
		err := l.Unlock()
		if err != nil {
			// TODO: log
		}
	}()

	err = sandboxfs.RemoveAll("")
	if err != nil {
		return nil, err
	}

	hashin, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, err
	}

	if def.Cache {
		res, ok, err := e.ResultFromLocalCache(ctx, def, def.Outputs, hashin)
		if err != nil {
			return nil, err
		}

		if ok {
			return res, nil
		}
	}

	// TODO: setup sandbox

	hashin2, err := e.hashin(ctx, def, results)
	if err != nil {
		return nil, err
	}

	if hashin != hashin2 {
		return nil, fmt.Errorf("modified while creating sandbox")
	}

	inputArtifacts := make([]*pluginv1.Artifact, 0)
	for _, result := range results {
		for _, output := range result.Outputs {
			inputArtifacts = append(inputArtifacts, output.Artifact)
		}
	}

	res, err := driver.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
		Target:      def.TargetDef,
		SandboxPath: sandboxfs.Path(),
		Inputs:      inputArtifacts,
		Pipes:       nil,
	}))
	if err != nil {
		return nil, err
	}

	cachefs := hfs.At(e.Cache, def.Ref.Package, "__"+def.Ref.Name, hashin)
	var execOutputs []ExecuteResultOutput

	for _, output := range def.CollectOutputs {
		tarname := "out_" + output.Group + ".tar"
		tarf, err := hfs.Create(cachefs, tarname)
		if err != nil {
			return nil, err
		}
		defer tarf.Close()

		tar := htar.NewPacker(tarf)
		for _, path := range output.Paths {
			// TODO: glob

			f, err := hfs.Open(sandboxfs, path)
			if err != nil {
				return nil, err
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

	for _, artifact := range res.Msg.Artifacts {
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

	// TODO: cleanup sandbox

	return &ExecuteResult{
		Hashin:  hashin,
		Outputs: execOutputs,
	}, nil
}

func (e *Engine) ExecuteAndCache(ctx context.Context, def *LightLinkedTarget) (*ExecuteResult, error) {
	res, err := e.Execute(ctx, def)
	if err != nil {
		return nil, err
	}

	// TODO: cache

	return res, nil
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
