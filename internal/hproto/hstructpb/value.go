package hstructpb

import "google.golang.org/protobuf/types/known/structpb"

func NewStringsValue(ss []string) *structpb.Value {
	values := make([]*structpb.Value, 0, len(ss))
	for _, s := range ss {
		values = append(values, structpb.NewStringValue(s))
	}

	return structpb.NewListValue(&structpb.ListValue{
		Values: values,
	})
}
