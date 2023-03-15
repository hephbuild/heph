package observability

import "context"

type duringGenKey struct{}

func IsDuringGen(ctx context.Context) bool {
	v, _ := ctx.Value(duringGenKey{}).(bool)
	return v
}
