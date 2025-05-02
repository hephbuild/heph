package hsync

import "sync"

func Go2[T1, T2 any](f func() (T1, T2)) func() (T1, T2) {
	ch1 := make(chan T1)
	ch2 := make(chan T2)

	go func() {
		defer close(ch1)
		defer close(ch2)

		v1, v2 := f()
		ch1 <- v1
		ch2 <- v2
	}()

	get := sync.OnceValues(func() (T1, T2) {
		v1 := <-ch1
		v2 := <-ch2

		return v1, v2
	})

	return get
}
