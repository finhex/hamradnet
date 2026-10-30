package main

import (
	"context"
	"errors"

	"github.com/finhex/hamradnet/internal/client"
)

// backend is what the window drives. Either the tunnel runs inside this app,
// which then needs root or Administrator, or it runs as the HamRadNet service
// and the app, running as an ordinary user, talks to it over the local control
// channel, the same one the command line uses.
type backend interface {
	Status() (client.Status, error)
	Peers() ([]client.PeerReport, error)
	Do(client.Request) client.Response
	// Log returns the recent log of whatever runs the tunnel.
	Log() (string, error)
	// Service reports whether the tunnel runs as the background service.
	Service() bool
	Close()
}

// embedded runs the tunnel in this process.
type embedded struct {
	daemon *client.Daemon
	cancel context.CancelFunc
	logs   *client.LogBuffer
}

func (e *embedded) Status() (client.Status, error)        { return e.daemon.Status(), nil }
func (e *embedded) Peers() ([]client.PeerReport, error)   { return e.daemon.PeerReports(), nil }
func (e *embedded) Do(req client.Request) client.Response { return e.daemon.Dispatch(req) }
func (e *embedded) Service() bool                         { return false }

func (e *embedded) Log() (string, error) {
	text, _, _ := e.logs.Snapshot()
	return text, nil
}

func (e *embedded) Close() {
	e.cancel()
	e.daemon.Close()
}

// service is the tunnel running as the HamRadNet background service.
type service struct{ path string }

// findService returns the running service, if there is one this user may
// drive. The install script sets it up so that the user who installed it can.
func findService() *service {
	s := &service{path: client.IPCPath()}
	if _, err := s.Status(); err != nil {
		return nil
	}
	return s
}

func (s *service) Status() (client.Status, error) {
	resp, err := client.SendCommand(s.path, client.Request{Command: "status"})
	if err != nil {
		return client.Status{}, err
	}
	if resp.Status == nil {
		return client.Status{}, errors.New(resp.Message)
	}
	return *resp.Status, nil
}

func (s *service) Peers() ([]client.PeerReport, error) {
	resp, err := client.SendCommand(s.path, client.Request{Command: "peers"})
	if err != nil {
		return nil, err
	}
	if !resp.OK {
		return nil, errors.New(resp.Message)
	}
	return resp.Peers, nil
}

func (s *service) Do(req client.Request) client.Response {
	resp, err := client.SendCommand(s.path, req)
	if err != nil {
		return client.Response{Message: "the HamRadNet service did not answer: " + err.Error()}
	}
	return resp
}

func (s *service) Log() (string, error) {
	resp := s.Do(client.Request{Command: "logs"})
	if !resp.OK {
		return "", errors.New(resp.Message)
	}
	return resp.Message, nil
}

func (s *service) Service() bool { return true }
func (s *service) Close()        {}
