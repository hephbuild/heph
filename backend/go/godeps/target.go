package main

import "fmt"

type Target struct {
	Name    string
	Package string
}

func (t Target) Full() string {
	return fmt.Sprintf("//%v:%v", t.Package, t.Name)
}
