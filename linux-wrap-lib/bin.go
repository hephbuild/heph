package lwl

import (
	"github.com/vmihailenco/msgpack/v5"
	"os"
	"path/filepath"
)

type Lib struct {
	Path    string
	Content []byte
}

type Bin struct {
	Libs []Lib
	Bin  []byte
}

func (b *Bin) Pack(path string) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()

	enc := msgpack.NewEncoder(f)
	enc.SetOmitEmpty(true)
	enc.UseArrayEncodedStructs(true)

	return enc.Encode(b)
}

func (b *Bin) Unpack(rootDir string) (string, error) {
	err := os.MkdirAll(rootDir, os.ModePerm)
	if err != nil {
		return "", err
	}

	for _, file := range b.Libs {
		err = os.WriteFile(filepath.Join(rootDir, filepath.Base(file.Path)), file.Content, os.ModePerm)
		if err != nil {
			return "", err
		}
	}

	binPath := filepath.Join(rootDir, "bin")

	err = os.WriteFile(binPath, b.Bin, os.ModePerm)
	if err != nil {
		return "", err
	}

	return binPath, nil
}

func Read(file string) (Bin, error) {
	f, err := os.Open(file)
	if err != nil {
		return Bin{}, err
	}
	defer f.Close()

	// TODO: remove shebang #!....\n
	//f.Read()

	var bin Bin
	err = msgpack.NewDecoder(f).Decode(&bin)
	if err != nil {
		return Bin{}, err
	}

	return bin, nil
}
