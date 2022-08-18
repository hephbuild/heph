package main

import (
	"encoding/json"
	"os"
)

var Env struct {
	Package string
	Sandbox string
}

var Config Cfg

func init() {
	err := json.Unmarshal([]byte(os.Args[3]), &Config)
	if err != nil {
		panic(err)
	}

	if Config.ThirdpartyPackage == "" {
		Config.ThirdpartyPackage = "thirdparty/go"
	}

	Env.Sandbox = os.Getenv("SANDBOX")
	Env.Package = os.Getenv("PACKAGE")
}

type Cfg struct {
	Test struct {
		Skip   []string `json:"skip"`
		PreRun string   `json:"pre_run"`
	} `json:"test"`
	Go                string `json:"go"`
	GoDeps            string `json:"godeps"`
	GenerateTestMain  string `json:"generate_testmain"`
	ThirdpartyPackage string `json:"thirdparty_package"`
}

func (c Cfg) IsTestSkipped(pkg string) bool {
	for _, s := range c.Test.Skip {
		if s == pkg {
			return true
		}
	}

	return false
}
