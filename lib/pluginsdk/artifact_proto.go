package pluginsdk

import (
	"archive/tar"
	"bytes"
	"fmt"
	"io"
	"os"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/htar"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
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

			tarPacker := htar.NewPacker(pw)
			tarPacker.AllowAbsLink = true // TODO: This is wrong., but let's ignore for now...

			f, err := hfs.Open(hfs.NewOS(content.GetSourcePath()))
			if err != nil {
				_ = pw.CloseWithError(err)

				return
			}
			defer f.Close()

			err = tarPacker.WriteFile(f, content.GetOutPath())
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

			tarPacker := htar.NewPacker(pw)

			err := tarPacker.Write(bytes.NewReader(content.GetData()), &tar.Header{
				Typeflag: tar.TypeReg,
				Name:     content.GetPath(),
				Size:     int64(len(content.GetData())),
				Mode:     mode,
			})
			if err != nil {
				_ = pw.CloseWithError(err)

				return
			}
		}()

		return pr, nil
	case pluginv1.Artifact_TargzPath_case:
		return os.Open(partifact.GetTargzPath())
	case pluginv1.Artifact_TarPath_case:
		return os.Open(partifact.GetTarPath())
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
	case pluginv1.Artifact_TargzPath_case:
		sourcefs := hfs.NewOS(partifact.GetTargzPath())

		info, err := sourcefs.Lstat()
		if err != nil {
			return 0, err
		}

		return info.Size(), nil
	case pluginv1.Artifact_TarPath_case:
		sourcefs := hfs.NewOS(partifact.GetTarPath())

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
		return ArtifactContentTypeTar, nil
	case pluginv1.Artifact_Raw_case:
		return ArtifactContentTypeTar, nil
	case pluginv1.Artifact_TargzPath_case:
		return ArtifactContentTypeTarGz, nil
	case pluginv1.Artifact_TarPath_case:
		return ArtifactContentTypeTar, nil
	default:
		return "", fmt.Errorf("unsupported encoding %v", partifact.WhichContent())
	}
}

func (a ProtoArtifact) GetName() string {
	partifact := a.Artifact

	switch partifact.WhichContent() {
	case pluginv1.Artifact_File_case, pluginv1.Artifact_Raw_case:
		return a.Artifact.GetName() + ".tar"
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
	case pluginv1.Artifact_TargzPath_case:
		return hfs.NewOS(partifact.GetTargzPath())
	case pluginv1.Artifact_TarPath_case:
		return hfs.NewOS(partifact.GetTarPath())
	default:
		return nil
	}
}
