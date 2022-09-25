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

func ParseConfig(cfgPath string) {
	cfg, err := os.ReadFile(cfgPath)
	if err != nil {
		panic(err)
	}

	err = json.Unmarshal(cfg, &Config)
	if err != nil {
		panic(err)
	}
}

func init() {
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
	StdPkgsTarget     string `json:"std_pkgs_target"`
	StdPkgsListFile   string `json:"std_pkgs_list_file"`
	ThirdpartyPackage string `json:"thirdparty_package"`
	BackendPkg        string `json:"backend_pkg"`
}

func (c Cfg) IsTestSkipped(pkg string) bool {
	for _, s := range c.Test.Skip {
		if s == pkg {
			return true
		}
	}

	return false
}
