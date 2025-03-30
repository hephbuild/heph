package plugingo

import "github.com/hephbuild/heph/plugin/plugingo/simpleregex"

var (
	matchThirdparty = simpleregex.MustNew("@go/thirdparty/{repo}@{version!/}", "@go/thirdparty/{repo}@{version!/}/{package}")
	matchGo         = simpleregex.MustNew("@go/toolchain/{version!/}")
	matchStd        = simpleregex.MustNew("@go/toolchain/{version!/}/std/{package}")
)

type Factors struct {
	GoVersion    string
	GOOS, GOARCH string
	Tags         string
}

func (f Factors) Args() map[string]string {
	return map[string]string{
		"os":   f.GOOS,
		"arch": f.GOARCH,
		"tags": f.Tags,
	}
}
