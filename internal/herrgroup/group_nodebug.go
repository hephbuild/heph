//go:build !errgroupdebug

package herrgroup

import (
	"context"

	"golang.org/x/sync/errgroup"
)

type Group = errgroup.Group

func WithContext(ctx context.Context) (Group, context.Context) {
	gp, ctx := errgroup.WithContext(ctx)

	return *gp, ctx //nolint:govet
}
