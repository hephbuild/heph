package engine

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/cloudclient"
	"github.com/hephbuild/heph/utils/fs"
	"os"
)

type CloudAuthData struct {
	Token     string
	UserID    string
	UserEmail string
}

const cloudAuthFile = "cloud_auth"

func (e *Engine) StoreCloudAuthData(data CloudAuthData) error {
	path := e.HomeDir.Join(cloudAuthFile).Abs()

	err := fs.CreateParentDir(path)
	if err != nil {
		return err
	}

	b, err := json.Marshal(data)
	if err != nil {
		return err
	}

	err = fs.WriteFileSync(path, b, os.ModePerm)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) GetCloudAuthData() (*CloudAuthData, error) {
	path := e.HomeDir.Join(cloudAuthFile).Abs()

	b, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, nil
		}
		return nil, err
	}

	var data CloudAuthData
	err = json.Unmarshal(b, &data)
	if err != nil {
		return nil, err
	}

	return &data, nil
}

func (e *Engine) DeleteCloudAuthData() error {
	path := e.HomeDir.Join(cloudAuthFile).Abs()

	return os.RemoveAll(path)
}

func (e *Engine) StartFlow(ctx context.Context, name string, metas map[string]string) (string, error) {
	if e.CloudClientAuth == nil {
		return "", fmt.Errorf("cloud not configured")
	}

	input := cloudclient.FlowInput{
		Name: name,
	}

	for k, v := range metas {
		input.Metas = append(input.Metas, cloudclient.FlowMetaInput{
			Key:   k,
			Value: v,
		})
	}

	res, err := cloudclient.RegisterFlow(ctx, e.CloudClientAuth, e.Config.Cloud.Project, input)
	if err != nil {
		return "", err
	}

	return res.RegisterFlow.Id, nil
}
