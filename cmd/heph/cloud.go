package main

import (
	"fmt"
	"github.com/hephbuild/heph/cloudclient"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/log/log"
	"github.com/spf13/cobra"
	"golang.org/x/term"
	"strings"
	"syscall"
)

var metas *[]string

func init() {
	cloudCmd.AddCommand(cloudLoginCmd)
	cloudCmd.AddCommand(cloudLogoutCmd)
	cloudCmd.AddCommand(cloudAuthActorCmd)
	cloudCmd.AddCommand(cloudStartFlowCmd)

	metas = cloudStartFlowCmd.PersistentFlags().StringArrayP("metas", "m", nil, "Flow meta key=value")
}

var cloudCmd = &cobra.Command{
	Use: "cloud",
}

var cloudLoginCmd = &cobra.Command{
	Use: "login",
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		err := engineInit(ctx)
		if err != nil {
			return err
		}

		if Engine.CloudClientAuth == nil {
			return fmt.Errorf("cloud not configured")
		}

		fmt.Println(Engine.Config.Cloud.URL)

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

		res, err := cloudclient.Login(ctx, Engine.CloudClient, email, pass)
		if err != nil {
			return err
		}

		tok := res.Login.Token
		user := res.Login.User

		err = Engine.StoreCloudAuthData(engine.CloudAuthData{
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
		return Engine.DeleteCloudAuthData()
	},
}

var cloudAuthActorCmd = &cobra.Command{
	Use: "auth-actor",
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		err := engineInit(ctx)
		if err != nil {
			return err
		}

		res, err := cloudclient.AuthActor(ctx, Engine.CloudClientAuth)
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
		ctx := cmd.Context()

		err := engineInit(ctx)
		if err != nil {
			return err
		}

		metasm := map[string]string{}
		for _, s := range *metas {
			parts := strings.SplitN(s, "=", 2)
			if len(parts) != 2 {
				return fmt.Errorf("malformed meta `%v`", s)
			}

			metasm[parts[0]] = parts[1]
		}

		id, err := Engine.StartFlow(ctx, args[0], metasm)
		if err != nil {
			return err
		}

		fmt.Println(id)

		return nil
	},
}
