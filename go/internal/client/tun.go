package client

import (
	"context"
	"fmt"
	"net/netip"
	"os/exec"
	"runtime"
	"sync"

	"github.com/finhex/hamradnet/internal/proto"
	"golang.zx2c4.com/wireguard/tun"
)

// tunDevice wraps the platform interface. wireguard-go's package covers both
// Linux's /dev/net/tun and Wintun behind one API, which is most of the reason
// this is pleasant in Go.
type tunDevice struct {
	dev       tun.Device
	name      string
	closeOnce sync.Once
}

func openTun(name string, mtu int) (*tunDevice, error) {
	dev, err := tun.CreateTUN(name, mtu)
	if err != nil {
		return nil, fmt.Errorf("creating the tunnel interface: %w", err)
	}
	actual, err := dev.Name()
	if err != nil {
		actual = name
	}
	return &tunDevice{dev: dev, name: actual}, nil
}

func (t *tunDevice) Close() {
	t.closeOnce.Do(func() { _ = t.dev.Close() })
}

// ensureTun brings the interface up, or re-addresses the existing one. The
// coordinator hands out a fresh address per connection, so a reconnect has to be
// able to move the interface rather than recreate it.
func (d *Daemon) ensureTun(ctx context.Context, identity *Identity) error {
	d.mu.Lock()
	existing := d.tun
	d.mu.Unlock()

	if existing != nil {
		if err := configureAddress(existing.name, identity.VirtualIP, identity.PrefixLen); err != nil {
			return fmt.Errorf("reassigning the tunnel address: %w", err)
		}
		d.log.Info("tunnel address updated", "address", identity.VirtualIP)
		return nil
	}

	device, err := openTun(d.interfaceName, proto.TunMTU)
	if err != nil {
		return err
	}
	if err := configureAddress(device.name, identity.VirtualIP, identity.PrefixLen); err != nil {
		device.Close()
		return err
	}
	d.log.Info("tunnel interface up", "interface", device.name, "address", identity.VirtualIP)

	d.mu.Lock()
	d.tun = device
	d.mu.Unlock()

	go d.tunReader(ctx, device)
	go d.tunWriter(ctx, device)
	return nil
}

// tunReader moves packets off the interface and onto the network.
func (d *Daemon) tunReader(ctx context.Context, device *tunDevice) {
	batch := device.dev.BatchSize()
	bufs := make([][]byte, batch)
	for i := range bufs {
		bufs[i] = make([]byte, proto.MaxPacketSize+offset)
	}
	sizes := make([]int, batch)

	// A run of failures means the device has gone; a single one means a packet
	// we could not take. Giving up on the first error would silently stop all
	// outbound traffic while the tunnel still looked healthy.
	consecutiveFailures := 0
	for ctx.Err() == nil {
		n, err := device.dev.Read(bufs, sizes, offset)
		if err != nil {
			consecutiveFailures++
			if consecutiveFailures >= readFailureLimit {
				d.log.Info("tun reader stopped: the device is gone", "err", err)
				return
			}
			continue
		}
		consecutiveFailures = 0
		for i := 0; i < n; i++ {
			packet := make([]byte, sizes[i])
			copy(packet, bufs[i][offset:offset+sizes[i]])
			d.flush(d.routeOutbound(packet))
		}
	}
}

// tunWriter moves decrypted packets onto the interface.
//
// One bad packet must never take the receive path down with it: returning on the
// first error would stop delivery for the lifetime of the process while
// handshakes and latency probes carried on working, so the tunnel would look
// perfectly healthy while no traffic arrived at all.
func (d *Daemon) tunWriter(ctx context.Context, device *tunDevice) {
	failures := 0
	buf := make([][]byte, 1)
	for {
		select {
		case <-ctx.Done():
			return
		case packet := <-d.toTun:
			framed := make([]byte, offset+len(packet))
			copy(framed[offset:], packet)
			buf[0] = framed
			if _, err := device.dev.Write(buf, offset); err != nil {
				failures++
				// Log the first, then ever more rarely, so a pathological peer
				// cannot flood the log.
				if failures&(failures-1) == 0 {
					d.log.Warn("dropping a packet the interface refused",
						"err", err, "failures", failures, "bytes", len(packet))
				}
			}
		}
	}
}

// offset is the headroom wireguard-go wants at the front of each buffer.
const offset = 16

// readFailureLimit is how many consecutive read errors mean the interface has
// gone, rather than one packet having been unreadable.
const readFailureLimit = 64

// routeOutbound decides where a packet from the interface goes.
func (d *Daemon) routeOutbound(packet []byte) []datagram {
	dst, ok := ipv4Destination(packet)
	if !ok {
		return nil
	}

	d.mu.Lock()
	defer d.mu.Unlock()

	if isFanout(dst) {
		// Copy to every peer, the way a switch floods a broadcast. This is what
		// makes "scan for LAN games" work.
		var out []datagram
		for _, id := range d.peers.IDs() {
			out = append(out, d.encryptForLocked(id, packet)...)
		}
		return out
	}

	id, ok := d.peers.ByVirtualIP(dst)
	if !ok {
		return nil
	}
	return d.encryptForLocked(id, packet)
}

// configureAddress assigns the interface address. Shelling out to the platform's
// own tool keeps this small and easy to check against what an administrator
// would type by hand.
func configureAddress(name string, addr netip.Addr, prefixLen uint8) error {
	switch runtime.GOOS {
	case "linux":
		// `replace` rather than `add`, so reassignment after a reconnect does
		// not leave the previous address behind.
		cidr := fmt.Sprintf("%s/%d", addr, prefixLen)
		if err := run("ip", "addr", "flush", "dev", name); err != nil {
			return err
		}
		if err := run("ip", "addr", "replace", cidr, "dev", name); err != nil {
			return err
		}
		return run("ip", "link", "set", "dev", name, "up")
	case "windows":
		return run("netsh", "interface", "ip", "set", "address",
			fmt.Sprintf("name=%s", name), "static", addr.String(), netmask(prefixLen))
	default:
		return fmt.Errorf("configuring an interface is not implemented on %s", runtime.GOOS)
	}
}

func run(program string, args ...string) error {
	out, err := exec.Command(program, args...).CombinedOutput()
	if err != nil {
		return fmt.Errorf("`%s %v` failed: %v: %s", program, args, err, out)
	}
	return nil
}

// netmask renders a prefix length as the dotted mask netsh expects.
func netmask(prefixLen uint8) string {
	var bits uint32
	switch {
	case prefixLen == 0:
		bits = 0
	case prefixLen >= 32:
		bits = ^uint32(0)
	default:
		bits = ^uint32(0) << (32 - prefixLen)
	}
	return netip.AddrFrom4([4]byte{
		byte(bits >> 24), byte(bits >> 16), byte(bits >> 8), byte(bits),
	}).String()
}
