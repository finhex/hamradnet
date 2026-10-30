package client

import (
	"strings"
	"sync"
)

// LogKeep is how many recent log lines a LogBuffer holds. Older ones are
// dropped; whatever else receives the log still has everything.
const LogKeep = 2000

// LogBuffer keeps the most recent log lines, so the app can show them and copy
// them for whoever is helping when something goes wrong. It is a second
// destination for a log, beside stderr. The daemon hands its lines out over
// the local control channel, which is how the app shows the log of a daemon
// running as a background service.
type LogBuffer struct {
	mu    sync.Mutex
	lines []string
	// added counts every line ever written, so a viewer can tell whether
	// anything is new even once old lines start being dropped.
	added int
}

// Write takes one log record; slog writes each in a single call.
func (b *LogBuffer) Write(p []byte) (int, error) {
	text := strings.TrimRight(string(p), "\n")
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, line := range strings.Split(text, "\n") {
		b.lines = append(b.lines, line)
		b.added++
	}
	if extra := len(b.lines) - LogKeep; extra > 0 {
		b.lines = append(b.lines[:0:0], b.lines[extra:]...)
	}
	return len(p), nil
}

// Snapshot returns the lines held, how many there are, and the running count
// they end at.
func (b *LogBuffer) Snapshot() (string, int, int) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return strings.Join(b.lines, "\n"), len(b.lines), b.added
}
