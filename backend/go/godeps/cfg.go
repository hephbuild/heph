package main

import (
	"encoding/json"
	"os"
	"sort"
	"strings"
)

var Env struct {
	Root    string
	Sandbox string
	Package string
	GOPATH  string
	GOOS    string
	GOARCH  string
}

var Config Cfg

var FilesOrigin map[string]string
var Deps map[string][]string

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

	Env.Root = os.Getenv("ROOT")
	Env.Sandbox = os.Getenv("SANDBOX")
	Env.Package = os.Getenv("PACKAGE")
	Env.GOPATH = goEnv("GOPATH")
	Env.GOOS = goEnv("GOOS")
	Env.GOARCH = goEnv("GOARCH")

	if p := os.Getenv("SRC_HEPH_FILES_ORIGIN"); p != "" {
		f, err := os.Open(p)
		if err != nil {
			panic(err)
		}
		defer f.Close()

		err = json.NewDecoder(f).Decode(&FilesOrigin)
		if err != nil {
			panic(err)
		}
	} else {
		panic("SRC_HEPH_FILES_ORIGIN is not defined")
	}

	if p := os.Getenv("SRC_HEPH_DEPS"); p != "" {
		f, err := os.Open(p)
		if err != nil {
			panic(err)
		}
		defer f.Close()

		err = json.NewDecoder(f).Decode(&Deps)
		if err != nil {
			panic(err)
		}
	} else {
		panic("SRC_HEPH_DEPS is not defined")
	}
}

type PkgCfgVariant struct {
	Name string `json:"name"`
	PkgCfgCompileVariant
	LDFlags string `json:"ldflags"`
}

type PkgCfgCompileVariant struct {
	OS   string   `json:"os"`
	ARCH string   `json:"arch"`
	Tags []string `json:"tags"`
}

type Extra map[string]interface{}

type PkgCfg struct {
	Test struct {
		Skip bool  `json:"skip"`
		Run  Extra `json:"run"`
	} `json:"test"`
	Variants []PkgCfgVariant `json:"variants"`
}

type Cfg struct {
	Pkg               map[string]PkgCfg `json:"pkg"`
	Replace           map[string]string `json:"replace"`
	Go                string            `json:"go"`
	GenerateTestMain  string            `json:"generate_testmain"`
	ThirdpartyPackage string            `json:"thirdparty_package"`
	BackendPkg        string            `json:"backend_pkg"`
}

func (c Cfg) GetPkgCfg(pkg string) PkgCfg {
	candidates := make([]string, 0)
	for matcher, cfg := range c.Pkg {
		if matcher == "..." {
			continue
		} else if strings.HasSuffix(matcher, "/...") {
			root := strings.TrimSuffix(matcher, "/...")

			if root == pkg || strings.HasPrefix(pkg, root+"/") {
				candidates = append(candidates, matcher)
			}
		} else {
			if matcher == pkg {
				return cfg
			}
		}
	}

	if len(candidates) == 0 {
		if cfg, ok := c.Pkg["..."]; ok {
			return cfg
		}

		// Default
		return PkgCfg{}
	}

	// Precision score
	candidateScore := func(candidate string) int {
		p := strings.Split(candidate, "/")
		return len(p)
	}

	sort.SliceStable(candidates, func(i, j int) bool {
		// sort reversed
		return candidateScore(candidates[i]) > candidateScore(candidates[j])
	})

	return c.Pkg[candidates[0]]
}
