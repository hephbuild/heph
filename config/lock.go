package config

import (
	"fmt"
	"heph/utils"
)

func Locker(cfg Config, p string) utils.Locker {
	switch cfg.Locker {
	case "flock":
		return utils.NewFlock(p)
	case "fslock":
		return utils.NewFSLock(p)
	default:
		panic(fmt.Errorf("unhandled locker `%v`", cfg.Locker))
	}
	return nil
}
