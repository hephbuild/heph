package targetspec

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

func mapEqual(a, b map[string]string) bool {
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

func (t TargetSpec) equalStruct(spec TargetSpec) bool {
	if t.Name != spec.Name {
		return false
	}

	if t.FQN != spec.FQN {
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

	if t.DifferentHashDeps != spec.DifferentHashDeps {
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

	if t.Gen != spec.Gen {
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

	return true
}

func (this TargetSpecCache) Equal(that TargetSpecCache) bool {
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

func (this TargetSpecTools) Equal(that TargetSpecTools) bool {
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

func (this TargetSpecDeps) Equal(that TargetSpecDeps) bool {
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

func (this TargetSpecDepFile) Equal(that TargetSpecDepFile) bool {
	if this.Path != that.Path {
		return false
	}

	if this.Name != that.Name {
		return false
	}

	return true
}

func (this TargetSpecDepExpr) Equal(that TargetSpecDepExpr) bool {
	if this.Expr.String != that.Expr.String {
		return false
	}

	if this.Name != that.Name {
		return false
	}

	return true
}

func (this TargetSpecExprTool) Equal(that TargetSpecExprTool) bool {
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

func (this TargetSpecTransitive) Equal(that TargetSpecTransitive) bool {
	if !this.Deps.Equal(that.Deps) {
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
