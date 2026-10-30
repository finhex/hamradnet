//go:build linux

package client

import (
	"context"
	"errors"
	"net"

	"golang.org/x/sys/unix"
)

// broadcastFilter runs in the kernel, so the daemon is only woken for what it
// wants: IPv4 packets on their way out, UDP, addressed to 255.255.255.255. The
// packet socket hands it each packet from the IP header on.
var broadcastFilter = []unix.SockFilter{
	{Code: 0x20, K: 0xfffff004},                         // A = packet type (SKF_AD_PKTTYPE)
	{Code: 0x15, Jt: 0, Jf: 7, K: unix.PACKET_OUTGOING}, // not outgoing: drop
	{Code: 0x20, K: 0xfffff000},                         // A = protocol (SKF_AD_PROTOCOL)
	{Code: 0x15, Jt: 0, Jf: 5, K: unix.ETH_P_IP},        // not IPv4: drop
	{Code: 0x30, K: 9},                                  // A = IP protocol
	{Code: 0x15, Jt: 0, Jf: 3, K: unix.IPPROTO_UDP},     // not UDP: drop
	{Code: 0x20, K: 16},                                 // A = destination address
	{Code: 0x15, Jt: 0, Jf: 1, K: 0xffffffff},           // not 255.255.255.255: drop
	{Code: 0x06, K: 0xffff},                             // keep
	{Code: 0x06, K: 0},                                  // drop
}

// captureBroadcasts copies games' LAN broadcasts into the tunnel until ctx
// ends; see rebroadcast for why. Everything else works without it, so where it
// cannot run it says so and steps aside.
func (d *Daemon) captureBroadcasts(ctx context.Context, tunName string) {
	tunIface, err := net.InterfaceByName(tunName)
	if err == nil {
		err = d.watchBroadcasts(ctx, tunIface.Index)
	}
	if err != nil {
		d.log.Warn("cannot copy LAN broadcasts into the tunnel; games that announce "+
			"themselves to 255.255.255.255 will not be seen by the other players", "err", err)
	}
}

func (d *Daemon) watchBroadcasts(ctx context.Context, tunIndex int) error {
	// Listening for every protocol is not a choice: only such sockets are shown
	// outgoing packets. The filter keeps the IPv4 ones.
	fd, err := unix.Socket(unix.AF_PACKET, unix.SOCK_DGRAM|unix.SOCK_CLOEXEC, int(htons(unix.ETH_P_ALL)))
	if err != nil {
		return err
	}
	defer unix.Close(fd)
	program := unix.SockFprog{Len: uint16(len(broadcastFilter)), Filter: &broadcastFilter[0]}
	if err := unix.SetsockoptSockFprog(fd, unix.SOL_SOCKET, unix.SO_ATTACH_FILTER, &program); err != nil {
		return err
	}
	// Wake up now and then, so the loop notices ctx ending.
	if err := unix.SetsockoptTimeval(fd, unix.SOL_SOCKET, unix.SO_RCVTIMEO, &unix.Timeval{Sec: 1}); err != nil {
		return err
	}
	d.log.Info("copying LAN game broadcasts into the tunnel")

	buf := make([]byte, 1<<16)
	for ctx.Err() == nil {
		n, from, err := unix.Recvfrom(fd, buf, 0)
		if errors.Is(err, unix.EAGAIN) || errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return err
		}
		link, ok := from.(*unix.SockaddrLinklayer)
		// What the tunnel itself sends out is already where it should be.
		if !ok || link.Pkttype != unix.PACKET_OUTGOING || link.Ifindex == tunIndex {
			continue
		}
		d.mu.Lock()
		identity := d.identity
		d.mu.Unlock()
		if identity == nil {
			continue
		}
		if packet, ok := rebroadcast(buf[:n], identity.VirtualIP); ok {
			d.flush(d.routeOutbound(packet))
		}
	}
	return nil
}

func htons(v uint16) uint16 { return v<<8 | v>>8 }
