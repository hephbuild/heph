package queue

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"testing"
	"time"
)

func testQueue(chunkSize int, concurrently bool, t *testing.T) {
	q := &Queue[int]{}
	expected := make([]int, 0)

	seed := func() {
		for i := 0; i < 1000; i++ {
			q.Enqueue(i)
			expected = append(expected, i)
			if concurrently {
				time.Sleep(time.Millisecond)
			}
		}
	}

	if concurrently {
		go seed()
		time.Sleep(100 * time.Millisecond)
	} else {
		seed()
	}

	result := make([]int, 0)
	err := q.DequeueChunk(chunkSize, func(vs []int) error {
		assert.LessOrEqual(t, len(vs), chunkSize)

		result = append(result, vs...)
		return nil
	})
	require.NoError(t, err)

	timeout := time.After(10 * time.Second)
forloop:
	for {
		select {
		case <-timeout:
			break forloop
		default:
			if len(result) == len(expected) {
				break forloop
			}
		}
	}

	assert.EqualValues(t, expected, result)
}

func TestQueueSmallChunk(t *testing.T) {
	testQueue(10, false, t)
}

func TestQueueLargeChunk(t *testing.T) {
	testQueue(10000, false, t)
}

func TestQueueConcurrently(t *testing.T) {
	testQueue(100, true, t)
}

func TestQueueWithError(t *testing.T) {
	q := &Queue[int]{}
	expected := make([]int, 0)

	for i := 0; i < 1000; i++ {
		q.Enqueue(i)
		expected = append(expected, i)
	}

	result := make([]int, 0)
	_ = q.DequeueChunk(10, func(vs []int) error {
		return fmt.Errorf("err")
	})
	_ = q.DequeueChunk(10, func(vs []int) error {
		result = append(result, vs...)
		return nil
	})
	_ = q.DequeueChunk(10, func(vs []int) error {
		return fmt.Errorf("err")
	})
	_ = q.DequeueChunk(10, func(vs []int) error {
		result = append(result, vs...)
		return nil
	})
	_ = q.DequeueChunk(10000, func(vs []int) error {
		result = append(result, vs...)
		return nil
	})

	assert.EqualValues(t, expected, result)
}

func TestQueueMax(t *testing.T) {
	q := &Queue[int]{Max: 5}
	expected := make([]int, 0)

	for i := 0; i < 10; i++ {
		q.Enqueue(i)
	}
	for i := 5; i < 10; i++ {
		expected = append(expected, i)
	}

	result := make([]int, 0)
	_ = q.DequeueChunk(10, func(vs []int) error {
		result = append(result, vs...)
		return nil
	})

	assert.EqualValues(t, expected, result)
}
