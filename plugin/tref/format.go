package tref

import (
	"fmt"
	"strings"

	"github.com/hephbuild/heph/internal/hmaps"

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
	var argsSuffix strings.Builder
	if len(ref.GetArgs()) > 0 {
		argsSuffix.WriteString("@")
		for k, v := range hmaps.Sorted(ref.GetArgs()) {
			if argsSuffix.String() != "@" {
				argsSuffix.WriteString(",")
			}

			argsSuffix.WriteString(k)
			argsSuffix.WriteString("=")
			if strings.ContainsAny(v, ` ,"'`) {
				argsSuffix.WriteString(fmt.Sprintf("%q", v))
			} else {
				argsSuffix.WriteString(v)
			}
		}
	}

	v := fmt.Sprintf("//%s:%s%s", ref.GetPackage(), ref.GetName(), argsSuffix.String())

	if ref, ok := ref.(RefableOut); ok {
		out := ref.GetOutput()
		if out != "" {
			v += "|" + out
		}
	}

	return v
}
