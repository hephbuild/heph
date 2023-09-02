package xprogress

import (
	"context"
	"time"
)

type Progress struct {
	n         float64
	size      float64
	estimated time.Time
}

// N gets the total number of bytes read or written
// so far.
func (p Progress) N() int64 {
	return int64(p.n)
}

func NewTicker(ctx context.Context, u *Counter, d time.Duration) <-chan Progress {
	var (
		started = time.Now()
		ch      = make(chan Progress)
	)
	go func() {
		defer close(ch)

		for {
			select {
			case <-ctx.Done():
				// context has finished - exit
				return
			case <-time.After(d):
				progress := Progress{
					n: float64(u.N()),
				}
				if progress.size > 0 {
					ratio := progress.n / progress.size
					past := float64(time.Since(started))
					if progress.n > 0.0 {
						total := time.Duration(past / ratio)
						if total < 168*time.Hour {
							// don't send estimates that are beyond a week
							progress.estimated = started.Add(total)
						}
					}
				}

				ch <- progress
			}
		}
	}()
	return ch
}
