package cmd

import (
	"runtime"
	"strconv"
	"strings"
)

type workersValue int

func newWorkersValue(p *int) *workersValue {
	*p = runtime.NumCPU()
	return (*workersValue)(p)
}

func (i *workersValue) Set(s string) error {
	isPercent := false
	if strings.HasSuffix(s, "%") {
		isPercent = true
		s = strings.TrimSuffix(s, "%")
	}

	v, err := strconv.ParseInt(s, 0, 64)
	if isPercent {
		*i = workersValue(float64(runtime.NumCPU()) * float64(v) / 100)
	} else {
		*i = workersValue(v)
	}

	return err
}

func (i *workersValue) Type() string {
	return "int"
}

func (i *workersValue) String() string { return strconv.Itoa(int(*i)) }
