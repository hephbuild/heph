package engine

import (
	"heph/utils"
	"sync"
)

type SrcRecorder struct {
	srcTar    []string
	src       []utils.TarFile
	namedSrc  map[string][]string
	srcOrigin map[string]string

	o sync.Once
}

func (s *SrcRecorder) init() {
	s.o.Do(func() {
		s.namedSrc = map[string][]string{}
		s.srcOrigin = map[string]string{}
	})
}

func (s *SrcRecorder) AddNamed(name, path, origin string) {
	a := s.namedSrc[name]
	a = append(a, path)
	s.namedSrc[name] = a

	if origin != "" {
		s.srcOrigin[path] = origin
	}
}

func (s *SrcRecorder) AddTar(tar string) {
	s.init()

	s.srcTar = append(s.srcTar, tar)
}

func (s *SrcRecorder) Add(name, from, to, origin string) {
	s.init()

	s.src = append(s.src, utils.TarFile{
		From: from,
		To:   to,
	})
	s.AddNamed(name, to, origin)
}

func (s *SrcRecorder) Origin() map[string]string {
	return s.srcOrigin
}

func (s *SrcRecorder) Src() []utils.TarFile {
	return s.src
}

func (s *SrcRecorder) SrcTar() []string {
	return s.srcTar
}

func (s *SrcRecorder) Named() map[string][]string {
	return s.namedSrc
}
