// Package client is the HamRadNet tunnel client: the identity, the peer table
// and the daemon that moves packets between the interface and the network.
package client

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"

	"github.com/finhex/hamradnet/internal/proto"
	"github.com/finhex/hamradnet/internal/session"
)

// SavedNetwork is a network to rejoin unattended after a restart. The password
// is kept for that, which is why the config file is owner-only.
type SavedNetwork struct {
	NetworkID string `json:"network_id"`
	Password  string `json:"password"`
}

// Config is what persists between runs.
type Config struct {
	// Coordinator address, host:port.
	Server string `json:"server"`
	// Name other members see in their peer list.
	Name       string         `json:"name"`
	PrivateKey string         `json:"private_key"`
	Networks   []SavedNetwork `json:"networks"`
}

// GenerateConfig creates a fresh identity.
func GenerateConfig(server, name string) (*Config, error) {
	private, _, err := session.GenerateKeypair()
	if err != nil {
		return nil, fmt.Errorf("generating an identity key: %w", err)
	}
	return &Config{
		Server:     server,
		Name:       name,
		PrivateKey: base64.StdEncoding.EncodeToString(private[:]),
	}, nil
}

// PrivateKeyBytes decodes the stored identity key.
func (c *Config) PrivateKeyBytes() ([session.KeyLen]byte, error) {
	var key [session.KeyLen]byte
	raw, err := base64.StdEncoding.DecodeString(c.PrivateKey)
	if err != nil {
		return key, err
	}
	if len(raw) != session.KeyLen {
		return key, fmt.Errorf("expected a %d-byte key, got %d", session.KeyLen, len(raw))
	}
	copy(key[:], raw)
	return key, nil
}

// PublicKey derives the public half of the identity.
func (c *Config) PublicKey() ([session.KeyLen]byte, error) {
	private, err := c.PrivateKeyBytes()
	if err != nil {
		return [session.KeyLen]byte{}, err
	}
	return session.PublicFromPrivate(private)
}

// LoadConfig reads the config file.
func LoadConfig(path string) (*Config, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var c Config
	if err := json.Unmarshal(raw, &c); err != nil {
		return nil, fmt.Errorf("parsing %s: %w", path, err)
	}
	return &c, nil
}

// Save writes the config, owner-only: it holds a private key and network
// passwords.
func (c *Config) Save(path string) error {
	if dir := filepath.Dir(path); dir != "" {
		// The folder is open, so the app can reach the local control file
		// when the daemon runs as a service; the files in it stay owner-only.
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
	}
	// The Rust client shares this file and refuses a null list, so an empty one
	// is written as [] - which a fresh config, never joined to anything, has.
	saved := *c
	if saved.Networks == nil {
		saved.Networks = []SavedNetwork{}
	}
	raw, err := json.MarshalIndent(saved, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, raw, 0o600)
}

// RememberNetwork records a network to rejoin at startup.
func (c *Config) RememberNetwork(networkID, password string) {
	for i := range c.Networks {
		if c.Networks[i].NetworkID == networkID {
			c.Networks[i].Password = password
			return
		}
	}
	c.Networks = append(c.Networks, SavedNetwork{NetworkID: networkID, Password: password})
}

// ForgetNetwork stops rejoining a network at startup.
func (c *Config) ForgetNetwork(networkID string) {
	kept := c.Networks[:0]
	for _, n := range c.Networks {
		if n.NetworkID != networkID {
			kept = append(kept, n)
		}
	}
	c.Networks = kept
}

// NormalizeServer tidies a server address as typed, adding the default port if
// there is none, since a bare IP is what people have to hand.
func NormalizeServer(server string) string {
	server = strings.TrimSpace(server)
	if server == "" {
		return ""
	}
	if _, _, err := net.SplitHostPort(server); err != nil {
		return net.JoinHostPort(strings.Trim(server, "[]"), strconv.Itoa(proto.DefaultControlPort))
	}
	return server
}

// DefaultConfigPath is where the config lives unless $HAMRADNET_CONFIG says
// otherwise. It is a system-wide location because the daemon needs elevated
// privileges to create the tunnel interface anyway.
func DefaultConfigPath() string {
	if custom := os.Getenv("HAMRADNET_CONFIG"); custom != "" {
		return custom
	}
	return filepath.Join(configDir(), "config.json")
}

// IPCPath is where the daemon advertises its local control port.
func IPCPath() string {
	if custom := os.Getenv("HAMRADNET_IPC"); custom != "" {
		return custom
	}
	return filepath.Join(configDir(), "ipc.json")
}

func configDir() string {
	if runtime.GOOS == "windows" {
		base := os.Getenv("ProgramData")
		if base == "" {
			base = `C:\ProgramData`
		}
		return filepath.Join(base, "HamRadNet")
	}
	return "/etc/hamradnet"
}

// EncodeKey renders a public key the way it is shown to users.
func EncodeKey(key [session.KeyLen]byte) string {
	return base64.StdEncoding.EncodeToString(key[:])
}
