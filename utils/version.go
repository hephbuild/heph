package utils

const DevVersion = "dev"

var Version = version

func IsDevVersion() bool {
	return Version == DevVersion
}
