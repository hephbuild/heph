package hlog

import (
	"fmt"
	"log/slog"
)

type pvhValue struct {
	v any
}

func (p pvhValue) LogValue() slog.Value {
	return slog.StringValue(fmt.Sprintf("%#v", p.v))
}

var _ slog.LogValuer = (*pvhValue)(nil)

// PHV stands for Percent Hashtag V, representing what it does.
func PHV(k string, v any) slog.Attr {
	return slog.Attr{
		Key:   k,
		Value: slog.AnyValue(pvhValue{v: v}),
	}
}
