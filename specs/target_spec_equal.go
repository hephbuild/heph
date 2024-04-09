package specs

func basicArrEqual[T any](a, b []T) bool {
	if (a == nil || b == nil) && (a != nil || b != nil) {
		return false
	}

	if len(a) != len(b) {
		return false
	}

	return true
}

func arrEqual[T comparable](a, b []T) bool {
	if !basicArrEqual(a, b) {
		return false
	}

	for i := 0; i < len(a); i++ {
		if a[i] != b[i] {
			return false
		}
	}

	return true
}

func arrEqualFunc[T interface{ Equal(T) bool }](a, b []T) bool {
	if !basicArrEqual(a, b) {
		return false
	}

	for i := 0; i < len(a); i++ {
		if !a[i].Equal(b[i]) {
			return false
		}
	}

	return true
}

func mapEqual[K, V comparable](a, b map[K]V) bool {
	if (a == nil || b == nil) && (a != nil || b != nil) {
		return false
	}

	if len(a) != len(b) {
		return false
	}

	for k := range a {
		if a[k] != b[k] {
			return false
		}
	}

	return true
}

func (t Target) equalStruct(spec Target) bool {
	if t.Name != spec.Name {
		return false
	}

	if t.Addr != spec.Addr {
		return false
	}

	if t.Package.Path != spec.Package.Path {
		return false
	}

	if !arrEqual(t.Run, spec.Run) {
		return false
	}

	if !arrEqual(t.FileContent, spec.FileContent) {
		return false
	}

	if t.Entrypoint != spec.Entrypoint {
		return false
	}

	if t.Dir != spec.Dir {
		return false
	}

	if t.PassArgs != spec.PassArgs {
		return false
	}

	if !t.Deps.Equal(spec.Deps) {
		return false
	}

	if !t.HashDeps.Equal(spec.HashDeps) {
		return false
	}

	if !t.RuntimeDeps.Equal(spec.RuntimeDeps) {
		return false
	}

	if !t.Transitive.Equal(spec.Transitive) {
		return false
	}

	if !t.Tools.Equal(spec.Tools) {
		return false
	}

	if !arrEqual(t.Out, spec.Out) {
		return false
	}

	if !t.Cache.Equal(spec.Cache) {
		return false
	}

	if t.Sandbox != spec.Sandbox {
		return false
	}

	if t.Codegen != spec.Codegen {
		return false
	}

	if !arrEqual(t.Labels, spec.Labels) {
		return false
	}

	if !mapEqual(t.Env, spec.Env) {
		return false
	}

	if !arrEqual(t.PassEnv, spec.PassEnv) {
		return false
	}

	if !arrEqual(t.RuntimePassEnv, spec.RuntimePassEnv) {
		return false
	}

	if t.RunInCwd != spec.RunInCwd {
		return false
	}

	if !arrEqual(t.Gen, spec.Gen) {
		return false
	}

	if !mapEqual(t.RuntimeEnv, spec.RuntimeEnv) {
		return false
	}

	if t.SrcEnv.Default != spec.SrcEnv.Default {
		return false
	}

	if !mapEqual(t.SrcEnv.Named, t.SrcEnv.Named) {
		return false
	}

	if t.OutEnv != spec.OutEnv {
		return false
	}

	if t.HashFile != spec.HashFile {
		return false
	}

	if !arrEqualFunc(t.Platforms, spec.Platforms) {
		return false
	}

	if !t.RestoreCache.Equal(spec.RestoreCache) {
		return false
	}

	if !mapEqual(t.Annotations, t.Annotations) {
		return false
	}

	if !mapEqual(t.Requests, t.Requests) {
		return false
	}

	return true
}

func (this Cache) Equal(that Cache) bool {
	if this.Enabled != that.Enabled {
		return false
	}

	if this.History != that.History {
		return false
	}

	if !arrEqual(this.Named, that.Named) {
		return false
	}

	return true
}

func (this Tools) Equal(that Tools) bool {
	if !arrEqual(this.Targets, that.Targets) {
		return false
	}

	if !arrEqual(this.Hosts, that.Hosts) {
		return false
	}

	if !arrEqualFunc(this.Exprs, that.Exprs) {
		return false
	}

	return true
}

func (this Deps) Equal(that Deps) bool {
	if !arrEqual(this.Targets, that.Targets) {
		return false
	}

	if !arrEqualFunc(this.Files, that.Files) {
		return false
	}

	if !arrEqualFunc(this.Exprs, that.Exprs) {
		return false
	}

	return true
}

func (this DepFile) Equal(that DepFile) bool {
	if this.Path != that.Path {
		return false
	}

	if this.Name != that.Name {
		return false
	}

	return true
}

func (this DepExpr) Equal(that DepExpr) bool {
	if this.Expr.String != that.Expr.String {
		return false
	}

	if this.Name != that.Name {
		return false
	}

	return true
}

func (this ExprTool) Equal(that ExprTool) bool {
	if this.Expr.String != that.Expr.String {
		return false
	}

	if this.Output != that.Output {
		return false
	}

	if this.Name != that.Name {
		return false
	}

	return true
}

func (this Transitive) Equal(that Transitive) bool {
	if !this.Deps.Equal(that.Deps) {
		return false
	}

	if !this.RuntimeDeps.Equal(that.RuntimeDeps) {
		return false
	}

	if !this.HashDeps.Equal(that.HashDeps) {
		return false
	}

	if !this.Tools.Equal(that.Tools) {
		return false
	}

	if !arrEqual(this.PassEnv, that.PassEnv) {
		return false
	}

	if !mapEqual(this.Env, that.Env) {
		return false
	}

	if !arrEqual(this.RuntimePassEnv, that.RuntimePassEnv) {
		return false
	}

	if !mapEqual(this.RuntimeEnv, that.RuntimeEnv) {
		return false
	}

	return true
}

func (this Platform) Equal(that Platform) bool {
	if !mapEqual(this.Options, that.Options) {
		return false
	}

	if !mapEqual(this.Labels, that.Labels) {
		return false
	}

	if this.Default != that.Default {
		return false
	}

	return true
}

func (this RestoreCache) Equal(that RestoreCache) bool {
	if this.Enabled != that.Enabled {
		return false
	}

	if this.Key != that.Key {
		return false
	}

	if !arrEqual(this.Paths, that.Paths) {
		return false
	}

	return true
}
