package xtypes

// ForceCast force casting any type by going through any
// It is pretty somewhat unsafe, beware!
func ForceCast[B any](a any) B {
	var b = a.(B)
	return b
}
