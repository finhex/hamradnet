package main

import (
	"fmt"
	"strings"
	"time"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/theme"
	"fyne.io/fyne/v2/widget"
)

func lines(n int) string {
	if n == 1 {
		return "1 line"
	}
	return fmt.Sprintf("%d lines", n)
}

func countLines(text string) int {
	if text == "" {
		return 0
	}
	return strings.Count(text, "\n") + 1
}

// logText is the log of whatever runs the tunnel: the service's own while the
// app drives it, otherwise this app's, which includes a tunnel run in-process.
func (u *ui) logText() (string, error) {
	if b := u.current(); b != nil && b.Service() {
		return b.Log()
	}
	text, _, _ := u.logs.Snapshot()
	return text, nil
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

	shown, first := "", true
	show := func(body string, err error) {
		if err != nil {
			info.SetText("Could not fetch the log: " + err.Error())
			return
		}
		if body == shown && !first {
			return
		}
		// Follow new lines only while the reader is at the bottom; someone who
		// scrolled up to read something must not be yanked away from it.
		bottom := scroll.Content.MinSize().Height - scroll.Size().Height
		following := first || scroll.Offset.Y >= bottom-theme.Padding()
		shown, first = body, false
		if body == "" {
			text.SetText("Nothing logged yet.")
		} else {
			text.SetText(body)
		}
		info.SetText(lines(countLines(body)))
		if following {
			scroll.ScrollToBottom()
		}
	}

	copyAll := widget.NewButton("Copy all", func() {
		window.Clipboard().SetContent(shown)
		info.SetText(fmt.Sprintf("Copied %s to the clipboard", lines(countLines(shown))))
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
	show(u.logText())
	window.Show()

	// Fetching the service's log is a request over the local control channel,
	// so it happens here rather than on the UI thread.
	go func() {
		ticker := time.NewTicker(refresh)
		defer ticker.Stop()
		for {
			select {
			case <-stop:
				return
			case <-ticker.C:
				body, err := u.logText()
				fyne.Do(func() { show(body, err) })
			}
		}
	}()
}
