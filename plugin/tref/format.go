package tref

import (
	"fmt"
	"github.com/hephbuild/heph/internal/hmaps"
	"strings"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

type Ref = pluginv1.TargetRef
type RefOut = pluginv1.TargetRefWithOutput

var _ Refable = (*Ref)(nil)
var _ Refable = (*RefOut)(nil)
var _ RefableOut = (*RefOut)(nil)

type Refable interface {
	GetArgs() map[string]string
	GetPackage() string
	GetName() string
}

type RefableOut interface {
	Refable
	GetOutput() string
}

func Format(ref Refable) string {
	var sb strings.Builder
	sb.WriteString("//")
	sb.WriteString(ref.GetPackage())
	sb.WriteString(":")
	sb.WriteString(ref.GetName())

	if len(ref.GetArgs()) > 0 {
		sb.WriteString("@")
		first := true
		for k, v := range hmaps.Sorted(ref.GetArgs()) {
			if !first {
				sb.WriteString(",")
			} else {
				first = false
			}

			sb.WriteString(k)
			sb.WriteString("=")
			if strings.ContainsAny(v, ` ,"'`) {
				sb.WriteString(fmt.Sprintf("%q", v))
			} else {
				sb.WriteString(v)
			}
		}
	}

	if ref, ok := ref.(RefableOut); ok {
		out := ref.GetOutput()
		if out != "" {
			sb.WriteString("|")
			sb.WriteString(out)
		}
	}

	return sb.String()
}
