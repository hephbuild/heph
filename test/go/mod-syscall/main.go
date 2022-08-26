package main

import (
	"fmt"
	"github.com/shirou/gopsutil/v3/process"
)

func Processes() []int32 {
	pids := make([]int32, 0)

	processes, err := process.Processes()
	if err != nil {
		panic(err)
	}
	for _, p := range processes {
		pids = append(pids, p.Pid)
	}

	return pids
}

func main() {
	fmt.Printf("Hello world :): %v processes\n", len(Processes()))
}
