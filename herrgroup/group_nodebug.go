//go:build !errgroupdebug

package herrgroup

import (
	"golang.org/x/sync/errgroup"
)

type Group = errgroup.Group
