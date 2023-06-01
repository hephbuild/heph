package bootstrap

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/cloudclient"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	obhephcloud "github.com/hephbuild/heph/observability/hephcloud"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
	"strings"
)

type Cloud struct {
	Root       *hroot.State
	Config     *config.Config
	Client     *cloudclient.HephClient
	AuthClient *cloudclient.HephClient
	Hook       *obhephcloud.Hook
}

type CloudAuthData struct {
	Token     string
	UserID    string
	UserEmail string
}

const cloudAuthFile = "cloud_auth"

func (c Cloud) GetAuthData() (*CloudAuthData, error) {
	path := c.Root.Home.Join(cloudAuthFile).Abs()

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

func (c Cloud) StoreCloudAuthData(data CloudAuthData) error {
	path := c.Root.Home.Join(cloudAuthFile).Abs()

	err := xfs.CreateParentDir(path)
	if err != nil {
		return err
	}

	b, err := json.Marshal(data)
	if err != nil {
		return err
	}

	err = xfs.WriteFileSync(path, b, os.ModePerm)
	if err != nil {
		return err
	}

	return nil
}

func (c Cloud) DeleteCloudAuthData() error {
	path := c.Root.Home.Join(cloudAuthFile).Abs()

	return os.RemoveAll(path)
}

type FlowDetails struct {
	ID  string
	URL string
}

func (c Cloud) currentFlowPath() string {
	return c.Root.Home.Join("tmp", "current_flow").Abs()
}

func (c Cloud) StartFlow(ctx context.Context, name string, metas map[string]string) error {
	if c.AuthClient == nil {
		return fmt.Errorf("cloud not configured")
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

	res, err := cloudclient.RegisterFlow(ctx, c.AuthClient, c.Config.Cloud.Project, input)
	if err != nil {
		return err
	}

	b, err := json.Marshal(FlowDetails{
		ID:  res.RegisterFlow.Id,
		URL: res.RegisterFlow.Url,
	})
	if err != nil {
		return err
	}

	err = xfs.CreateParentDir(c.currentFlowPath())
	if err != nil {
		return err
	}

	err = os.WriteFile(c.currentFlowPath(), b, os.ModePerm)
	if err != nil {
		return err
	}

	return nil
}

func (c Cloud) StopCurrentFlow(ctx context.Context) error {
	return os.RemoveAll(c.currentFlowPath())
}

func (c Cloud) GetCurrentFlowDetails(ctx context.Context) (FlowDetails, error) {
	b, err := os.ReadFile(c.currentFlowPath())
	if err != nil {
		return FlowDetails{}, err
	}

	var deets FlowDetails
	err = json.Unmarshal(b, &deets)
	if err != nil {
		return FlowDetails{}, err
	}

	return deets, nil
}

func setupHephcloud(ctx context.Context, root *hroot.State, cfg *config.Config, fins *finalizers.Finalizers, obs *observability.Observability, installObs bool, flowId string) (Cloud, error) {
	cloud := Cloud{Root: root, Config: cfg}

	if cfg.Cloud.URL != "" && cfg.Cloud.Project != "" {
		cloudClient := cloudclient.New(strings.TrimRight(cfg.Cloud.URL, "/") + "/api/graphql")
		cloud.Client = &cloudClient

		token := strings.TrimSpace(os.Getenv("HEPH_CLOUD_TOKEN"))
		if token == "" {
			data, err := cloud.GetAuthData()
			if err != nil {
				return Cloud{}, fmt.Errorf("cloud auth: %w", err)
			}

			if data != nil {
				token = data.Token
			}
		}

		if token == "" {
			log.Errorf("You must login to use cloud features")
		} else {
			client := cloudClient.WithAuthToken(token)
			cloud.AuthClient = &client

			if installObs {
				_, err := cloudclient.AuthActor(ctx, client)
				if err != nil {
					log.Errorf("You must login to use cloud features: auth error: %v", err)
				} else {
					hook := obhephcloud.NewHook(&obhephcloud.Hook{
						Client:    client,
						ProjectID: cfg.Cloud.Project,
						Config:    cfg,
						FlowId:    flowId,
					})
					cloud.Hook = hook
					obs.RegisterHook(hook)

					flush := hook.Start(ctx)
					fins.Register(func() {
						flush()
					})
				}
			}
		}
	}

	return cloud, nil
}
