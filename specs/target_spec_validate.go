package specs

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xfs"
	"strings"
)

func joinOneOf[T ~string](valid []T) string {
	valids := ads.Map(valid, func(t T) string {
		return string(t)
	})
	for i, s := range valid {
		valids[i] = fmt.Sprintf("`%v`", s)
	}

	return strings.Join(valids, ", ")
}

func (t Target) Validate() error {
	for _, target := range t.Deps.Targets {
		if !ads.Contains(DepModes, target.Mode) {
			return fmt.Errorf("deps.%v must be one of %v, got %v", target.Name, joinOneOf(DepModes), target.Mode)
		}
	}

	for k, v := range t.SrcEnv.Named {
		if !ads.Contains(FileEnvValues, v) {
			return fmt.Errorf("src_env[%v] must be one of %v, got %v", k, joinOneOf(FileEnvValues), v)
		}
	}

	if !ads.Contains(FileEnvValues, t.SrcEnv.Default) {
		return fmt.Errorf("src_env must be one of %v, got %v", joinOneOf(FileEnvValues), t.SrcEnv.Default)
	}

	if !ads.Contains(HashFileValues, t.HashFile) {
		return fmt.Errorf("hash_file must be one of %v, got %v", joinOneOf(HashFileValues), t.HashFile)
	}

	if !ads.Contains(FileEnvValues, t.RestoreCache.Env) {
		return fmt.Errorf("restore_cache.env must be one of %v, got %v", joinOneOf(FileEnvValues), t.RestoreCache.Env)
	}

	if !ads.Contains(FileEnvValues, t.OutEnv) {
		return fmt.Errorf("out_env must be one of %v, got %v", joinOneOf(FileEnvValues), t.OutEnv)
	}

	if !ads.Contains(CodegenValues, t.Codegen) {
		return fmt.Errorf("codegen must be one of %v, got %v", joinOneOf(CodegenValues), t.Codegen)
	}

	if !ads.Contains(EntrypointValues, t.Entrypoint) {
		return fmt.Errorf("entrypoint must be one of %v, got %v", joinOneOf(EntrypointValues), t.Entrypoint)
	}

	if len(t.Platforms) != 1 {
		return fmt.Errorf("only a single platform is supported, for now")
	}

	if t.Codegen != CodegenNone {
		if !t.Sandbox {
			return fmt.Errorf("codegen is only suported in sandboxed targets")
		}

		for _, file := range t.Out {
			if xfs.IsGlob(file.Path) {
				return fmt.Errorf("codegen targets must not have glob outputs")
			}
		}
	}

	for _, label := range t.Labels {
		err := LabelValidate(label)
		if err != nil {
			return err
		}
	}

	if t.Cache.Enabled && t.ConcurrentExecution {
		return fmt.Errorf("concurrent_execution and cache are incompatible")
	}

	if !t.Cache.Enabled && t.RestoreCache.Enabled {
		return fmt.Errorf("restore_cache requires cache to be enabled")
	}

	if t.Cache.Enabled && t.RunInCwd {
		return fmt.Errorf("run in cwd is incompatble with cache")
	}

	return nil
}
