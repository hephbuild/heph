package sandbox

func sysProcAttr() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{
		Pdeathsig:  syscall.SIGHUP,
		Setpgid:    true,
		Foreground: foreground,
	}
}
