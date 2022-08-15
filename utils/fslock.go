package utils

import (
	"bytes"
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"os"
	"path/filepath"
	"sync"
	"time"
)

func NewFSLock(p string) *FSLock {
	return &FSLock{path: p}
}

type FSLock struct {
	m    sync.Mutex
	path string
}

func (l *FSLock) Lock() error {
	l.m.Lock()

	if dir := filepath.Dir(l.path); dir != "." {
		err := os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return err
		}
	}

	for {
		err := l.acquire()
		if err != nil {
			if !errors.Is(err, os.ErrExist) {
				log.Errorf("lock: %v", err)
			}
			time.Sleep(500 * time.Millisecond)
			continue
		}

		return nil
	}
}

func (l *FSLock) acquire() error {
	f, err := os.OpenFile(l.path, os.O_RDWR|os.O_CREATE|os.O_TRUNC|os.O_SYNC|os.O_EXCL, os.ModePerm)
	if err != nil {
		return err
	}
	_, err = f.Write([]byte(ID))
	if err1 := f.Sync(); err1 != nil && err == nil {
		err = err1
	}
	if err1 := f.Close(); err1 != nil && err == nil {
		err = err1
	}
	if err != nil {
		return err
	}

	time.Sleep(time.Second)

	return l.has()
}

func (l *FSLock) has() error {
	b, err := os.ReadFile(l.path)
	if err != nil {
		return err
	}

	if !bytes.Equal(b, []byte(ID)) {
		return fmt.Errorf("found %s, expected %s", b, ID)
	}

	return nil
}

func (l *FSLock) Unlock() error {
	l.m.Unlock()

	err := l.has()
	if err != nil {
		return err
	}

	return l.Clean()
}

func (l *FSLock) Clean() error {
	err := os.RemoveAll(l.path)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	return nil
}
