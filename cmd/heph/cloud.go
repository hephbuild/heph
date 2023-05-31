package main

import (
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/cloudclient"
	"github.com/hephbuild/heph/log/log"
	"github.com/spf13/cobra"
	"golang.org/x/term"
	"syscall"
)

var metas *[]string

func init() {
	cloudCmd.AddCommand(cloudLoginCmd)
	cloudCmd.AddCommand(cloudLogoutCmd)
	cloudCmd.AddCommand(cloudAuthActorCmd)
	cloudCmd.AddCommand(cloudStartFlowCmd)
	cloudCmd.AddCommand(cloudStopFlowCmd)
	cloudCmd.AddCommand(cloudFlowGetCmd)

	metas = cloudStartFlowCmd.PersistentFlags().StringArrayP("metas", "m", nil, "Flow meta key=value")
}

var cloudCmd = &cobra.Command{
	Use: "cloud",
}

var cloudLoginCmd = &cobra.Command{
	Use: "login",
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := bootstrapInit(ctx)
		if err != nil {
			return err
		}

		if bs.Cloud.Client == nil {
			return fmt.Errorf("cloud not configured")
		}

		fmt.Println(bs.Config.Cloud.URL)

		fmt.Print("Email: ")
		var email string
		_, err = fmt.Scanln(&email)
		if err != nil {
			return err
		}

		fmt.Print("Password: ")
		passb, err := term.ReadPassword(syscall.Stdin)
		fmt.Println()
		if err != nil {
			return err
		}
		pass := string(passb)

		res, err := cloudclient.Login(ctx, bs.Cloud.Client, email, pass)
		if err != nil {
			return err
		}

		tok := res.Login.Token
		user := res.Login.User

		err = bs.Cloud.StoreCloudAuthData(bootstrap.CloudAuthData{
			Token:     tok,
			UserID:    user.Id,
			UserEmail: user.Email,
		})
		if err != nil {
			return err
		}

		log.Infof("Successfully authenticated as %v", user.Email)

		return nil
	},
}

var cloudLogoutCmd = &cobra.Command{
	Use: "logout",
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := bootstrapInit(ctx)
		if err != nil {
			return err
		}

		return bs.Cloud.DeleteCloudAuthData()
	},
}

var cloudAuthActorCmd = &cobra.Command{
	Use: "auth-actor",
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := bootstrapInit(ctx)
		if err != nil {
			return err
		}

		res, err := cloudclient.AuthActor(ctx, bs.Cloud.AuthClient)
		if err != nil {
			return err
		}

		actor := res.Auth.Actor

		if actor == nil {
			return fmt.Errorf("no actor")
		}

		log.Info(actor.GetActor_type(), actor.GetActor_id())

		return nil
	},
}

var cloudStartFlowCmd = &cobra.Command{
	Use:  "start-flow",
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		//ctx := cmd.Context()
		//
		//bs, err := bootstrapInit(ctx)
		//if err != nil {
		//	return err
		//}
		//
		//metasm := map[string]string{}
		//for _, s := range *metas {
		//	parts := strings.SplitN(s, "=", 2)
		//	if len(parts) != 2 {
		//		return fmt.Errorf("malformed meta `%v`", s)
		//	}
		//
		//	metasm[parts[0]] = parts[1]
		//}
		//
		//err = Engine.StartFlow(ctx, args[0], metasm)
		//if err != nil {
		//	return err
		//}

		return nil
	},
}

var cloudStopFlowCmd = &cobra.Command{
	Use:  "stop-flow",
	Args: cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		//ctx := cmd.Context()
		//
		//bs, err := bootstrapInit(ctx)
		//if err != nil {
		//	return err
		//}
		//
		//err = Engine.StopCurrentFlow(ctx)
		//if err != nil {
		//	return err
		//}

		return nil
	},
}

var cloudFlowGetCmd = &cobra.Command{
	Use:  "get",
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		//ctx := cmd.Context()
		//
		//bs, err := bootstrapInit(ctx)
		//if err != nil {
		//	return err
		//}
		//
		//flow, err := Engine.GetCurrentFlowDetails(ctx)
		//if err != nil {
		//	return err
		//}
		//
		//switch args[0] {
		//case "id":
		//	fmt.Println(flow.ID)
		//case "url":
		//	fmt.Println(flow.URL)
		//default:
		//	return fmt.Errorf("invalid flow attribute %v", args[0])
		//}

		return nil
	},
}
