package xtypes

import "reflect"

// ForceCast force casting any type by going through any
// It is pretty somewhat unsafe, beware!
func ForceCast[B any](a any) B {
	var b = a.(B)
	return b
}

// IsNil allows to go around nil interfaces
// see https://stackoverflow.com/a/78104852/3212099
func IsNil(input interface{}) bool {
	if input == nil {
		return true
	}
	kind := reflect.ValueOf(input).Kind()
	switch kind {
	case reflect.Ptr, reflect.Map, reflect.Slice, reflect.Chan:
		return reflect.ValueOf(input).IsNil()
	default:
		return false
	}
}
