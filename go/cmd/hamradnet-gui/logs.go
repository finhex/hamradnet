package main

import (
	"fmt"
	"strings"
	"sync"
	"time"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/theme"
	"fyne.io/fyne/v2/widget"
)

// logKeep is how many recent log lines the app holds for the log window. Older
// ones are dropped; the terminal still has everything.
const logKeep = 2000

// logBuffer keeps the most recent log lines so the app can show them, and copy
// them for whoever is helping when something goes wrong. It sits beside
// stderr as a second destination for the same log.
type logBuffer struct {
	mu    sync.Mutex
	lines []string
	// added counts every line ever written, so a viewer can tell whether
	// anything is new even once old lines start being dropped.
	added int
}

// Write takes one log record; slog writes each in a single call.
func (b *logBuffer) Write(p []byte) (int, error) {
	text := strings.TrimRight(string(p), "\n")
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, line := range strings.Split(text, "\n") {
		b.lines = append(b.lines, line)
		b.added++
	}
	if extra := len(b.lines) - logKeep; extra > 0 {
		b.lines = append(b.lines[:0:0], b.lines[extra:]...)
	}
	return len(p), nil
}

// Snapshot returns the lines held, and the running count they end at.
func (b *logBuffer) Snapshot() (string, int, int) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return strings.Join(b.lines, "\n"), len(b.lines), b.added
}

func lines(n int) string {
	if n == 1 {
		return "1 line"
	}
	return fmt.Sprintf("%d lines", n)
}

// showLogs opens the log in a window of its own, so it can sit beside the main
// one. It follows new lines as they come, and Copy puts the whole log on the
// clipboard, ready to paste into a chat or an issue.
func (u *ui) showLogs() {
	if u.logWindow != nil {
		u.logWindow.RequestFocus()
		return
	}
	window := fyne.CurrentApp().NewWindow("HamRadNet log")
	window.Resize(fyne.NewSize(900, 520))

	text := widget.NewLabel("")
	text.TextStyle = fyne.TextStyle{Monospace: true}
	text.Selectable = true
	scroll := container.NewScroll(text)
	info := widget.NewLabel("")

	shown := -1
	update := func() {
		body, count, added := u.logs.Snapshot()
		if added == shown {
			return
		}
		// Follow new lines only while the reader is at the bottom; someone who
		// scrolled up to read something must not be yanked away from it.
		bottom := scroll.Content.MinSize().Height - scroll.Size().Height
		following := shown < 0 || scroll.Offset.Y >= bottom-theme.Padding()
		shown = added
		if body == "" {
			body = "Nothing logged yet."
		}
		text.SetText(body)
		info.SetText(lines(count))
		if following {
			scroll.ScrollToBottom()
		}
	}

	copyAll := widget.NewButton("Copy all", func() {
		body, count, _ := u.logs.Snapshot()
		window.Clipboard().SetContent(body)
		info.SetText(fmt.Sprintf("Copied %s to the clipboard", lines(count)))
	})
	copyAll.Importance = widget.HighImportance

	window.SetContent(container.NewBorder(
		nil,
		container.NewHBox(copyAll, widget.NewButton("Close", window.Close), info),
		nil, nil,
		scroll,
	))

	stop := make(chan struct{})
	window.SetOnClosed(func() {
		close(stop)
		u.logWindow = nil
	})
	u.logWindow = window
	update()
	window.Show()

	go func() {
		ticker := time.NewTicker(refresh)
		defer ticker.Stop()
		for {
			select {
			case <-stop:
				return
			case <-ticker.C:
				fyne.Do(update)
			}
		}
	}()
}
