package targetrun

import (
	"github.com/hephbuild/heph/utils/tar"
	"sync"
)

type SrcRecorder struct {
	srcTar    []string
	src       []tar.File
	namedSrc  map[string][]string
	srcOrigin map[string]string

	o sync.Once

	Parent *SrcRecorder
}

func (s *SrcRecorder) init() {
	s.o.Do(func() {
		s.namedSrc = map[string][]string{}
		s.srcOrigin = map[string]string{}
	})
}

func (s *SrcRecorder) addNamed(name, path, origin string) {
	a := s.namedSrc[name]
	a = append(a, path)
	s.namedSrc[name] = a

	if origin != "" {
		s.srcOrigin[path] = origin
	}
}

func (s *SrcRecorder) AddTar(tar string) {
	s.init()

	for _, itar := range s.srcTar {
		if itar == tar {
			return
		}
	}

	s.srcTar = append(s.srcTar, tar)
}

func (s *SrcRecorder) Add(name, from, to, origin string) {
	s.init()

	s.src = append(s.src, tar.File{
		From: from,
		To:   to,
	})
	s.addNamed(name, to, origin)

	if s.Parent != nil {
		s.Parent.Add(name, from, to, origin)
	}
}

func (s *SrcRecorder) Origin() map[string]string {
	return s.srcOrigin
}

func (s *SrcRecorder) Src() []tar.File {
	return s.src
}

func (s *SrcRecorder) SrcTar() []string {
	return s.srcTar
}

func (s *SrcRecorder) Named() map[string][]string {
	return s.namedSrc
}
