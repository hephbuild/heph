package pluginsdk

import (
	"bytes"
	"fmt"
	"io"
	"os"

	"github.com/hephbuild/heph/internal/hcpio"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/unikraft/go-cpio"
)

type ProtoArtifact struct {
	*pluginv1.Artifact
}

var _ Artifact = (*ProtoArtifact)(nil)

func (a ProtoArtifact) GetContentReader() (io.ReadCloser, error) {
	partifact := a.Artifact

	switch partifact.WhichContent() {
	case pluginv1.Artifact_File_case:
		content := partifact.GetFile()

		pr, pw := io.Pipe()

		go func() {
			defer pw.Close()

			packer := hcpio.NewPacker(pw)
			packer.AllowAbsLink = true // TODO: This is wrong., but let's ignore for now...

			f, err := hfs.Open(hfs.NewOS(content.GetSourcePath()))
			if err != nil {
				_ = pw.CloseWithError(err)

				return
			}
			defer f.Close()

			err = packer.WriteFile(f, content.GetOutPath())
			if err != nil {
				_ = pw.CloseWithError(err)

				return
			}
		}()

		return pr, nil
	case pluginv1.Artifact_Raw_case:
		content := partifact.GetRaw()

		pr, pw := io.Pipe()

		go func() {
			defer pw.Close()

			mode := int64(os.ModePerm)
			if content.GetX() {
				mode |= 0111 // executable
			}

			cpioPacker := hcpio.NewPacker(pw)

			err := cpioPacker.Write(bytes.NewReader(content.GetData()), &cpio.Header{
				Mode: cpio.FileMode(mode) | cpio.TypeRegular,
				Name: content.GetPath(),
				Size: int64(len(content.GetData())),
			})
			if err != nil {
				_ = pw.CloseWithError(err)

				return
			}
		}()

		return pr, nil
	case pluginv1.Artifact_TarPath_case:
		return os.Open(partifact.GetTarPath())
	case pluginv1.Artifact_CpioPath_case:
		return os.Open(partifact.GetCpioPath())
	default:
		return nil, fmt.Errorf("unsupported encoding %v", partifact.WhichContent())
	}
}

func (a ProtoArtifact) GetContentSize() (int64, error) {
	partifact := a.Artifact

	switch partifact.WhichContent() {
	case pluginv1.Artifact_File_case:
		content := partifact.GetFile()

		sourcefs := hfs.NewOS(content.GetSourcePath())

		info, err := sourcefs.Lstat()
		if err != nil {
			return 0, err
		}

		return info.Size(), nil
	case pluginv1.Artifact_Raw_case:
		content := partifact.GetRaw()

		return int64(len(content.GetData())), nil
	case pluginv1.Artifact_TarPath_case:
		sourcefs := hfs.NewOS(partifact.GetTarPath())

		info, err := sourcefs.Lstat()
		if err != nil {
			return 0, err
		}

		return info.Size(), nil
	case pluginv1.Artifact_CpioPath_case:
		sourcefs := hfs.NewOS(partifact.GetCpioPath())

		info, err := sourcefs.Lstat()
		if err != nil {
			return 0, err
		}

		return info.Size(), nil
	default:
		return -1, fmt.Errorf("unsupported encoding %v", partifact.WhichContent())
	}
}

func (a ProtoArtifact) GetContentType() (ArtifactContentType, error) {
	partifact := a.Artifact

	switch partifact.WhichContent() {
	case pluginv1.Artifact_File_case:
		return ArtifactContentTypeCpio, nil
	case pluginv1.Artifact_Raw_case:
		return ArtifactContentTypeCpio, nil
	case pluginv1.Artifact_TarPath_case:
		return ArtifactContentTypeTar, nil
	case pluginv1.Artifact_CpioPath_case:
		return ArtifactContentTypeCpio, nil
	default:
		return "", fmt.Errorf("unsupported encoding %v", partifact.WhichContent())
	}
}

func (a ProtoArtifact) GetName() string {
	partifact := a.Artifact

	switch partifact.WhichContent() {
	case pluginv1.Artifact_File_case, pluginv1.Artifact_Raw_case:
		return a.Artifact.GetName() + ".cpio"
	default:
		return a.Artifact.GetName()
	}
}

func (a ProtoArtifact) FSNode() hfs.Node {
	partifact := a.Artifact

	switch partifact.WhichContent() {
	case pluginv1.Artifact_File_case:
		return hfs.NewOS(partifact.GetFile().GetSourcePath())
	case pluginv1.Artifact_Raw_case:
		return nil
	case pluginv1.Artifact_TarPath_case:
		return hfs.NewOS(partifact.GetTarPath())
	case pluginv1.Artifact_CpioPath_case:
		return hfs.NewOS(partifact.GetCpioPath())
	default:
		return nil
	}
}
