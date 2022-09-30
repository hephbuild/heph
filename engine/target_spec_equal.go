package engine

func arrEqual[T comparable](a, b []T) bool {
	if (a == nil || b == nil) && (a != nil || b != nil) {
		return false
	}

	if len(a) != len(b) {
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
	if len(a) != len(b) {
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

	if t.Package.FullName != spec.Package.FullName {
		return false
	}

	if !arrEqual(t.Run, spec.Run) {
		return false
	}

	if t.Executor != spec.Executor {
		return false
	}

	if t.Quiet != spec.Quiet {
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

	if !arrEqualFunc(t.ExprTools, spec.ExprTools) {
		return false
	}

	if !arrEqual(t.TargetTools, spec.TargetTools) {
		return false
	}

	if !arrEqual(t.HostTools, spec.HostTools) {
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

	if t.RunInCwd != spec.RunInCwd {
		return false
	}

	if t.Gen != spec.Gen {
		return false
	}

	if !mapEqual(t.RuntimeEnv, spec.RuntimeEnv) {
		return false
	}

	if t.SrcEnv != spec.SrcEnv {
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

	if this.Package.FullName != that.Package.FullName {
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

	if this.Package.FullName != that.Package.FullName {
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

	return true
}
