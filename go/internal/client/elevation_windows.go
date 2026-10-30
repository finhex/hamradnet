//go:build windows

package client

import (
	"os"
	"path/filepath"
)

func isElevated() bool {
	probe := filepath.Join(os.Getenv("SystemRoot"), "hamradnet-elevation-probe.tmp")
	f, err := os.Create(probe)
	if err != nil {
		return false
	}
	_ = f.Close()
	_ = os.Remove(probe)
	return true
}
