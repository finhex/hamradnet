//go:build !linux

package client

import "context"

// captureBroadcasts is Linux-only. On Windows the tunnel is made the interface
// broadcasts prefer instead; see ensureTun.
func (d *Daemon) captureBroadcasts(ctx context.Context, tunName string) {}
