package clean

// Sum adds through a channel handoff, so the two goroutines are properly
// ordered and the race detector has nothing to report.
func Sum(a, b int) int {
	ch := make(chan int)
	go func() { ch <- a }()
	return <-ch + b
}
