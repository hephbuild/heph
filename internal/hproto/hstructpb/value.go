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

func NewMapStringStringValue(m map[string]string) *structpb.Value {
	values := make(map[string]*structpb.Value, len(m))
	for k, v := range m {
		values[k] = structpb.NewStringValue(v)
	}

	return structpb.NewStructValue(&structpb.Struct{
		Fields: values,
	})
}

func NewMapStringStringsValue(m map[string][]string) *structpb.Value {
	values := make(map[string]*structpb.Value, len(m))
	for k, v := range m {
		values[k] = NewStringsValue(v)
	}

	return structpb.NewStructValue(&structpb.Struct{
		Fields: values,
	})
}
