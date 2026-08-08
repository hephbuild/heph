package racy

// Increment deliberately races: the spawned goroutine and the caller both write
// `n` with no synchronisation between them. The channel only signals completion,
// so it orders the final read but not the two writes.
func Increment() int {
	n := 0
	done := make(chan struct{})
	go func() {
		n++
		close(done)
	}()
	n++
	<-done
	return n
}
