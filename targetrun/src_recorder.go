package targetrun

import (
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/utils/tar"
	"sync"
)

type SrcRecorder struct {
	srcTar    []sandbox.TarFile
	src       []tar.File
	namedSrc  map[string][]string
	srcOrigin map[string]string

	o sync.Once

	Forward *SrcRecorder
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

func (s *SrcRecorder) AddTar(tar string, size int64) {
	s.init()

	for _, itar := range s.srcTar {
		if itar.Path == tar {
			return
		}
	}

	s.srcTar = append(s.srcTar, sandbox.TarFile{
		Path: tar,
		Size: size,
	})
}

func (s *SrcRecorder) Add(name, from, to, origin string) {
	s.init()

	s.src = append(s.src, sandbox.File{
		From: from,
		To:   to,
	})
	s.addNamed(name, to, origin)

	if s.Forward != nil {
		s.Forward.Add(name, from, to, origin)
	}
}

func (s *SrcRecorder) Origin() map[string]string {
	return s.srcOrigin
}

func (s *SrcRecorder) Src() []sandbox.File {
	return s.src
}

func (s *SrcRecorder) SrcTar() []sandbox.TarFile {
	return s.srcTar
}

func (s *SrcRecorder) Named() map[string][]string {
	return s.namedSrc
}

func (s *SrcRecorder) Reset() {
	s.srcTar = nil
	s.src = nil
	s.namedSrc = nil
	s.srcOrigin = nil
	s.o = sync.Once{}

	if s.Forward != nil {
		panic("SrcRecorder.Reset called with Forward, undefined behavior")
	}
}
