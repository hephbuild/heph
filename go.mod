module github.com/hephbuild/heph

go 1.21

replace github.com/spf13/cobra v1.7.0 => github.com/raphaelvigee/cobra v0.0.0-20221020122344-217ca52feee0

require (
	cloud.google.com/go/storage v1.34.1
	github.com/Khan/genqlient v0.6.0
	github.com/aarondl/json v0.0.0-20221020222930-8b0db17ef1bf
	github.com/aarondl/opt v0.0.0-20230313190023-85d93d668fec
	github.com/bep/debounce v1.2.1
	github.com/blevesearch/bleve/v2 v2.3.10
	github.com/blevesearch/bleve_index_api v1.1.2
	github.com/bmatcuk/doublestar/v4 v4.6.1
	github.com/c2fo/vfs/v6 v6.9.0
	github.com/charmbracelet/bubbles v0.16.1
	github.com/charmbracelet/bubbletea v0.26.1
	github.com/charmbracelet/lipgloss v0.9.1
	github.com/coreos/go-semver v0.3.1
	github.com/creack/pty v1.1.20
	github.com/dlsniper/debugger v0.6.0
	github.com/fsnotify/fsnotify v1.7.0
	github.com/google/uuid v1.4.0
	github.com/heimdalr/dag v1.3.1
	github.com/hinshun/vt10x v0.0.0-20220301184237-5011da428d02
	github.com/itchyny/gojq v0.12.13
	github.com/lithammer/fuzzysearch v1.1.8
	github.com/mattn/go-isatty v0.0.20
	github.com/mitchellh/go-homedir v1.1.0
	github.com/muesli/reflow v0.3.0
	github.com/muesli/termenv v0.15.2
	github.com/olekukonko/tablewriter v0.0.5
	github.com/pbnjay/memory v0.0.0-20210728143218-7b4eea64cf58
	github.com/shirou/gopsutil/v3 v3.23.10
	github.com/spf13/cobra v1.7.0
	github.com/spf13/pflag v1.0.5
	github.com/stretchr/testify v1.8.4
	github.com/viney-shih/go-lock v1.1.2
	github.com/vmihailenco/msgpack/v5 v5.4.1
	github.com/zeebo/xxh3 v1.0.2
	go.opentelemetry.io/otel v1.19.0
	go.opentelemetry.io/otel/exporters/jaeger v1.17.0
	go.opentelemetry.io/otel/sdk v1.19.0
	go.opentelemetry.io/otel/trace v1.19.0
	go.starlark.net v0.0.0-20231101134539-556fd59b42f6
	go.uber.org/multierr v1.11.0
	golang.org/x/exp v0.0.0-20231006140011-7918f672742d
	golang.org/x/net v0.17.0
	golang.org/x/sys v0.19.0
	golang.org/x/term v0.19.0
	google.golang.org/api v0.149.0
	gopkg.in/yaml.v3 v3.0.1
)

require (
	cloud.google.com/go v0.110.10 // indirect
	cloud.google.com/go/compute v1.23.3 // indirect
	cloud.google.com/go/compute/metadata v0.2.3 // indirect
	cloud.google.com/go/iam v1.1.5 // indirect
	github.com/RoaringBitmap/roaring v1.6.0 // indirect
	github.com/agnivade/levenshtein v1.1.1 // indirect
	github.com/alexflint/go-arg v1.4.3 // indirect
	github.com/alexflint/go-scalar v1.2.0 // indirect
	github.com/atotto/clipboard v0.1.4 // indirect
	github.com/aws/aws-sdk-go v1.47.3 // indirect
	github.com/aymanbagabas/go-osc52/v2 v2.0.1 // indirect
	github.com/bits-and-blooms/bitset v1.11.0 // indirect
	github.com/blevesearch/geo v0.1.18 // indirect
	github.com/blevesearch/go-porterstemmer v1.0.3 // indirect
	github.com/blevesearch/gtreap v0.1.1 // indirect
	github.com/blevesearch/mmap-go v1.0.4 // indirect
	github.com/blevesearch/scorch_segment_api/v2 v2.2.2 // indirect
	github.com/blevesearch/segment v0.9.1 // indirect
	github.com/blevesearch/snowballstem v0.9.0 // indirect
	github.com/blevesearch/upsidedown_store_api v1.0.2 // indirect
	github.com/blevesearch/vellum v1.0.10 // indirect
	github.com/blevesearch/zapx/v11 v11.3.10 // indirect
	github.com/blevesearch/zapx/v12 v12.3.10 // indirect
	github.com/blevesearch/zapx/v13 v13.3.10 // indirect
	github.com/blevesearch/zapx/v14 v14.3.10 // indirect
	github.com/blevesearch/zapx/v15 v15.3.13 // indirect
	github.com/davecgh/go-spew v1.1.1 // indirect
	github.com/emirpasic/gods v1.18.1 // indirect
	github.com/erikgeiser/coninput v0.0.0-20211004153227-1c3628e74d0f // indirect
	github.com/go-logr/logr v1.3.0 // indirect
	github.com/go-logr/stdr v1.2.2 // indirect
	github.com/go-ole/go-ole v1.3.0 // indirect
	github.com/golang/geo v0.0.0-20230421003525-6adc56603217 // indirect
	github.com/golang/groupcache v0.0.0-20210331224755-41bb18bfe9da // indirect
	github.com/golang/protobuf v1.5.3 // indirect
	github.com/golang/snappy v0.0.4 // indirect
	github.com/google/s2a-go v0.1.7 // indirect
	github.com/googleapis/enterprise-certificate-proxy v0.3.2 // indirect
	github.com/googleapis/gax-go/v2 v2.12.0 // indirect
	github.com/inconshreveable/mousetrap v1.1.0 // indirect
	github.com/itchyny/timefmt-go v0.1.5 // indirect
	github.com/jmespath/go-jmespath v0.4.0 // indirect
	github.com/json-iterator/go v1.1.12 // indirect
	github.com/klauspost/cpuid/v2 v2.2.5 // indirect
	github.com/kr/fs v0.1.0 // indirect
	github.com/kr/pretty v0.3.1 // indirect
	github.com/lucasb-eyer/go-colorful v1.2.0 // indirect
	github.com/lufia/plan9stats v0.0.0-20231016141302-07b5767bb0ed // indirect
	github.com/mattn/go-localereader v0.0.1 // indirect
	github.com/mattn/go-runewidth v0.0.15 // indirect
	github.com/modern-go/concurrent v0.0.0-20180306012644-bacd9c7ef1dd // indirect
	github.com/modern-go/reflect2 v1.0.2 // indirect
	github.com/mschoch/smat v0.2.0 // indirect
	github.com/muesli/ansi v0.0.0-20230316100256-276c6243b2f6 // indirect
	github.com/muesli/cancelreader v0.2.2 // indirect
	github.com/pkg/errors v0.9.1 // indirect
	github.com/pkg/sftp v1.13.6 // indirect
	github.com/pmezard/go-difflib v1.0.0 // indirect
	github.com/power-devops/perfstat v0.0.0-20221212215047-62379fc7944b // indirect
	github.com/rivo/uniseg v0.4.6 // indirect
	github.com/sahilm/fuzzy v0.1.0 // indirect
	github.com/shoenig/go-m1cpu v0.1.6 // indirect
	github.com/stretchr/objx v0.5.1 // indirect
	github.com/tklauser/go-sysconf v0.3.12 // indirect
	github.com/tklauser/numcpus v0.6.1 // indirect
	github.com/vektah/gqlparser/v2 v2.5.10 // indirect
	github.com/vmihailenco/tagparser/v2 v2.0.0 // indirect
	github.com/yusufpapurcu/wmi v1.2.3 // indirect
	go.etcd.io/bbolt v1.3.8 // indirect
	go.opencensus.io v0.24.0 // indirect
	go.opentelemetry.io/otel/metric v1.19.0 // indirect
	golang.org/x/crypto v0.14.0 // indirect
	golang.org/x/mod v0.14.0 // indirect
	golang.org/x/oauth2 v0.13.0 // indirect
	golang.org/x/sync v0.7.0 // indirect
	golang.org/x/text v0.14.0 // indirect
	golang.org/x/tools v0.14.0 // indirect
	golang.org/x/xerrors v0.0.0-20231012003039-104605ab7028 // indirect
	google.golang.org/appengine v1.6.8 // indirect
	google.golang.org/genproto v0.0.0-20231030173426-d783a09b4405 // indirect
	google.golang.org/genproto/googleapis/api v0.0.0-20231030173426-d783a09b4405 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20231030173426-d783a09b4405 // indirect
	google.golang.org/grpc v1.59.0 // indirect
	google.golang.org/protobuf v1.31.0 // indirect
	gopkg.in/check.v1 v1.0.0-20201130134442-10cb98267c6c // indirect
	gopkg.in/yaml.v2 v2.4.0 // indirect
)
