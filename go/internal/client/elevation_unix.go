//go:build !windows

package client

import "os"

func isElevated() bool { return os.Geteuid() == 0 }
