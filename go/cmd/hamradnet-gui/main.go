// Command hamradnet-gui is the HamRadNet desktop app.
//
// One program that is both the window and the tunnel: the daemon runs in this
// same process, so there is nothing to start separately and nothing to type.
package main

import (
	"context"
	"fmt"
	"image/color"
	"io"
	"log/slog"
	"os"
	"runtime"
	"sort"
	"strings"
	"sync"
	"time"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/app"
	"fyne.io/fyne/v2/canvas"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/dialog"
	"fyne.io/fyne/v2/layout"
	"fyne.io/fyne/v2/theme"
	"fyne.io/fyne/v2/widget"

	"github.com/finhex/hamradnet/internal/client"
)

// refresh is how often the window redraws, so peer state looks live.
const refresh = time.Second

var version = "0.4.0-go"

type ui struct {
	window fyne.Window
	log    *slog.Logger
	// logs holds recent log lines for the log window; logWindow is that window
	// while it is open.
	logs      *client.LogBuffer
	logWindow fyne.Window

	mu         sync.Mutex
	backend    backend
	configPath string

	// Setup screen.
	serverEntry *widget.Entry
	nameEntry   *widget.Entry
	setupError  *widget.Label

	// Main screen.
	// statusDot is drawn rather than written: the bundled font has no "●".
	statusDot   *canvas.Circle
	statusLabel *widget.Label
	addressText *widget.Label
	content     *fyne.Container
	networkBox  *fyne.Container
	notice      *widget.Label
}

func main() {
	// sudo drops DISPLAY and WAYLAND_DISPLAY, so the window cannot be opened and
	// the app would die with an error from deep inside the toolkit. Say what
	// actually went wrong, and how to fix it.
	if runtime.GOOS == "linux" &&
		os.Getenv("DISPLAY") == "" && os.Getenv("WAYLAND_DISPLAY") == "" {
		fmt.Fprint(os.Stderr, `HamRadNet cannot open a window: no display is available.

This is almost always `+"`sudo`"+` stripping the display settings.
`+"`sudo`"+` on its own will not work here. Use the launcher instead:

    ./run-gui.sh

or, by hand:

    sudo -E env -u WAYLAND_DISPLAY -u XDG_RUNTIME_DIR \
        DISPLAY=$DISPLAY XAUTHORITY=$XAUTHORITY hamradnet-gui

The app needs root to create the network interface, but it also needs your
session's display to draw the window.
`)
		os.Exit(1)
	}

	// Everything logged goes to the terminal as before, and is also kept for the
	// app's own log window.
	logs := &client.LogBuffer{}
	log := slog.New(slog.NewTextHandler(io.MultiWriter(os.Stderr, logs),
		&slog.HandlerOptions{Level: slog.LevelInfo}))
	log.Info("starting HamRadNet", "version", version, "os", runtime.GOOS)

	application := app.NewWithID("net.hamradnet.app")
	window := application.NewWindow("HamRadNet")
	window.Resize(fyne.NewSize(920, 640))

	u := &ui{window: window, log: log, logs: logs, configPath: client.DefaultConfigPath()}
	u.build()
	window.SetCloseIntercept(func() {
		u.shutdown()
		if u.logWindow != nil {
			u.logWindow.Close()
		}
		window.Close()
	})
	window.ShowAndRun()
}

func (u *ui) build() {
	existing, err := client.LoadConfig(u.configPath)

	u.serverEntry = widget.NewEntry()
	u.serverEntry.SetPlaceHolder("203.0.113.45")
	u.nameEntry = widget.NewEntry()
	u.nameEntry.SetPlaceHolder("what your friends will see")
	if err == nil {
		u.serverEntry.SetText(existing.Server)
		u.nameEntry.SetText(existing.Name)
	} else {
		u.nameEntry.SetText(defaultName())
	}
	u.setupError = widget.NewLabel("")
	u.setupError.Wrapping = fyne.TextWrapWord

	u.statusDot = canvas.NewCircle(connectingColor)
	u.statusLabel = widget.NewLabel("")
	u.addressText = widget.NewLabel("")
	u.notice = widget.NewLabel("")
	u.notice.Wrapping = fyne.TextWrapWord
	u.networkBox = container.NewVBox()

	u.content = container.NewStack()
	u.window.SetContent(u.content)

	switch svc := findService(); {
	case svc != nil:
		// The tunnel already runs as a service: drive that, with no root needed.
		u.log.Info("found the HamRadNet service; this app controls it")
		u.backend = svc
		status, _ := svc.Status()
		u.nameEntry.SetText(status.Name)
		if status.Server == "" {
			u.showSetup()
		} else {
			u.serverEntry.SetText(status.Server)
			u.showMain()
		}
	case err == nil && client.IsElevated():
		// Already set up from a previous run: connect without asking again.
		u.showMain()
		u.connect()
	default:
		u.showSetup()
	}
	go u.refreshLoop()
}

// isService reports whether this app is driving the background service.
func (u *ui) isService() bool {
	b := u.current()
	return b != nil && b.Service()
}

func (u *ui) showSetup() {
	title := canvas.NewText("HamRadNet", theme.Color(theme.ColorNameForeground))
	title.TextSize = 28
	title.TextStyle = fyne.TextStyle{Bold: true}

	form := widget.NewForm(
		widget.NewFormItem("Server address", u.serverEntry),
		widget.NewFormItem("Your name", u.nameEntry),
	)
	connect := widget.NewButton("Connect", func() { u.connect() })
	connect.Importance = widget.HighImportance

	items := []fyne.CanvasObject{
		container.NewCenter(title),
		container.NewCenter(widget.NewLabel("A private LAN for playing games with friends.")),
		widget.NewSeparator(),
	}
	if u.isService() {
		items = append(items, widget.NewLabel(
			"HamRadNet runs as a service on this computer. Choose a server to connect it to."))
	} else if !client.IsElevated() {
		items = append(items, elevationWarning())
	}
	items = append(items,
		form,
		widget.NewLabel("Just the IP of your server. Port 7777 is assumed."),
		container.NewCenter(container.NewHBox(connect,
			widget.NewButton("Log", func() { u.showLogs() }))),
		u.setupError,
	)

	u.content.Objects = []fyne.CanvasObject{
		container.NewPadded(container.NewVBox(items...)),
	}
	u.content.Refresh()
}

func (u *ui) showMain() {
	header := container.NewBorder(nil, nil,
		container.NewHBox(
			container.NewCenter(container.NewGridWrap(fyne.NewSize(10, 10), u.statusDot)),
			u.statusLabel, u.addressText),
		container.NewHBox(
			widget.NewButton("Log", func() { u.showLogs() }),
			widget.NewButton("Change server", func() { u.disconnect() }),
		),
	)

	buttons := container.NewHBox(
		layout.NewSpacer(),
		widget.NewButton("Create a network", func() { u.createDialog() }),
		widget.NewButton("Join a network", func() { u.joinDialog() }),
	)

	body := container.NewVBox(
		container.NewBorder(nil, nil, widget.NewLabelWithStyle(
			"Networks", fyne.TextAlignLeading, fyne.TextStyle{Bold: true}), buttons),
		u.networkBox,
	)

	u.content.Objects = []fyne.CanvasObject{
		container.NewPadded(container.NewBorder(
			container.NewVBox(header, widget.NewSeparator()),
			container.NewVBox(widget.NewSeparator(), u.notice),
			nil, nil,
			container.NewVScroll(body),
		)),
	}
	u.content.Refresh()
}

func elevationWarning() fyne.CanvasObject {
	what := "Close it and start it again with the run-gui.sh launcher, which asks for root, " +
		"or install the HamRadNet service once (install.sh in the service download), " +
		"after which the app needs no password."
	if runtime.GOOS == "windows" {
		what = "Close it, right-click hamradnet-gui.exe and choose \"Run as administrator\"."
	}
	title := widget.NewLabelWithStyle("This app needs administrator rights",
		fyne.TextAlignLeading, fyne.TextStyle{Bold: true})
	detail := widget.NewLabel(what + "\nCreating a network adapter is not permitted otherwise.")
	detail.Wrapping = fyne.TextWrapWord
	return container.NewVBox(title, detail, widget.NewSeparator())
}

// connect points the service at the chosen server, or, with no service, creates
// or updates the config and starts the daemon in this process.
func (u *ui) connect() {
	// A bare IP is the common case, so the port is filled in rather than making
	// people remember it.
	server := client.NormalizeServer(u.serverEntry.Text)
	if server == "" {
		u.setupError.SetText("Enter your server's address first.")
		return
	}
	name := strings.TrimSpace(u.nameEntry.Text)
	if name == "" {
		name = defaultName()
	}

	if u.isService() {
		resp := u.current().Do(client.Request{Command: "configure", Server: server, Name: name})
		if !resp.OK {
			u.setupError.SetText(resp.Message)
			return
		}
		u.setupError.SetText("")
		u.showMain()
		return
	}

	config, err := client.LoadConfig(u.configPath)
	if err != nil {
		config, err = client.GenerateConfig(server, name)
		if err != nil {
			u.setupError.SetText("Could not create an identity: " + err.Error())
			return
		}
	} else {
		config.Server, config.Name = server, name
	}
	if err := config.Save(u.configPath); err != nil {
		u.setupError.SetText("Could not save settings: " + err.Error())
		return
	}

	daemon := client.NewDaemon(config, u.configPath, u.log)
	daemon.SetLogBuffer(u.logs)
	ctx, cancel := context.WithCancel(context.Background())

	u.mu.Lock()
	u.backend = &embedded{daemon: daemon, cancel: cancel, logs: u.logs}
	u.mu.Unlock()

	go func() {
		if err := daemon.ServeIPC(); err != nil {
			u.log.Error("local control channel stopped", "err", err)
		}
	}()
	go func() {
		if err := daemon.Run(ctx, 0); err != nil {
			u.log.Error("daemon stopped", "err", err)
		}
	}()

	u.setupError.SetText("")
	u.showMain()
}

// disconnect returns to the setup screen so the server can be changed without
// restarting. A tunnel in this process is stopped; the service keeps running
// until it is given the new server.
func (u *ui) disconnect() {
	if u.isService() {
		if status, err := u.current().Status(); err == nil {
			u.serverEntry.SetText(status.Server)
			u.nameEntry.SetText(status.Name)
		}
	} else {
		u.shutdown()
		if config, err := client.LoadConfig(u.configPath); err == nil {
			u.serverEntry.SetText(config.Server)
			u.nameEntry.SetText(config.Name)
		}
	}
	u.notice.SetText("")
	u.showSetup()
}

// shutdown stops a tunnel running in this process. The service is left alone:
// closing the app does not take anyone off the network.
func (u *ui) shutdown() {
	u.mu.Lock()
	b := u.backend
	if b != nil && !b.Service() {
		u.backend = nil
	}
	u.mu.Unlock()
	if b != nil && !b.Service() {
		b.Close()
	}
}

func (u *ui) current() backend {
	u.mu.Lock()
	defer u.mu.Unlock()
	return u.backend
}

func (u *ui) refreshLoop() {
	for range time.Tick(refresh) {
		b := u.current()
		if b == nil {
			continue
		}
		status, err := b.Status()
		if err != nil {
			fyne.Do(func() { u.notice.SetText("The HamRadNet service is not answering: " + err.Error()) })
			continue
		}
		peers, _ := b.Peers()
		fyne.Do(func() { u.render(status, peers) })
	}
}

var (
	connectedColor  = color.NRGBA{R: 70, G: 190, B: 120, A: 255}
	connectingColor = color.NRGBA{R: 220, G: 160, B: 60, A: 255}
)

func (u *ui) render(status client.Status, peers []client.PeerReport) {
	state, dot := "Connecting…", connectingColor
	if status.Connected {
		state, dot = "Connected", connectedColor
	}
	if u.statusDot.FillColor != dot {
		u.statusDot.FillColor = dot
		u.statusDot.Refresh()
	}
	u.statusLabel.SetText(fmt.Sprintf("%s · %s", state, status.Server))
	if status.VirtualIP != "" {
		u.addressText.SetText("   Your address: " + status.VirtualIP)
	}
	if status.LastError != "" && !status.Connected {
		u.notice.SetText("Cannot reach the server: " + status.LastError)
	}

	sort.Slice(status.Networks, func(i, j int) bool {
		return status.Networks[i].NetworkID < status.Networks[j].NetworkID
	})

	cards := make([]fyne.CanvasObject, 0, len(status.Networks))
	for _, network := range status.Networks {
		cards = append(cards, u.networkCard(network, status, peers))
	}
	if len(cards) == 0 {
		cards = append(cards, widget.NewLabel(
			"You are not in a network yet.\nCreate one and share the ID, or join a friend's."))
	}
	u.networkBox.Objects = cards
	u.networkBox.Refresh()
}

// networkCard renders one network with everyone in it, so it reads as a room
// rather than one flat list of everybody.
func (u *ui) networkCard(network client.NetworkReport, status client.Status, peers []client.PeerReport) fyne.CanvasObject {
	members := make([]client.PeerReport, 0, len(peers))
	for _, peer := range peers {
		for _, id := range peer.Networks {
			if id == network.NetworkID {
				members = append(members, peer)
				break
			}
		}
	}
	sort.Slice(members, func(i, j int) bool { return members[i].VirtualIP < members[j].VirtualIP })
	total := len(members) + 1

	id := network.NetworkID
	title := widget.NewLabelWithStyle(network.Name, fyne.TextAlignLeading, fyne.TextStyle{Bold: true})
	idLabel := widget.NewLabelWithStyle(id, fyne.TextAlignLeading, fyne.TextStyle{Monospace: true})
	count := widget.NewLabel(fmt.Sprintf("%d player%s", total, plural(total)))

	actions := container.NewHBox(
		count,
		widget.NewButton("Copy ID", func() {
			u.window.Clipboard().SetContent(id)
			u.notice.SetText("Network ID copied.")
		}),
		widget.NewButton("Leave", func() { u.send(client.Request{Command: "leave", NetworkID: id}) }),
		widget.NewButton("Delete", func() { u.confirmDelete(id, network.Name) }),
	)

	rows := []fyne.CanvasObject{
		container.NewBorder(nil, nil, container.NewVBox(title, idLabel), actions),
		widget.NewSeparator(),
		memberRow("Address", "Name", "Status", "Ping", "Traffic", true),
		memberRow(status.VirtualIP, status.Name+" (you)", stateWord(status.Connected),
			pingText(status.ServerRTTMs), traffic(status.TotalRX, status.TotalTX), false),
	}
	for _, peer := range members {
		rows = append(rows, memberRow(peer.VirtualIP, peer.Name, peer.Link,
			pingText(peer.RTTMs), traffic(peer.RXBytes, peer.TXBytes), false))
	}
	if len(members) == 0 {
		rows = append(rows, widget.NewLabel("Nobody else here yet - send a friend the ID above."))
	}
	return widget.NewCard("", "", container.NewVBox(rows...))
}

func memberRow(address, name, state, ping, bytes string, header bool) fyne.CanvasObject {
	style := fyne.TextStyle{}
	if header {
		style = fyne.TextStyle{Bold: true}
	}
	cell := func(text string, mono bool) fyne.CanvasObject {
		s := style
		s.Monospace = mono && !header
		return widget.NewLabelWithStyle(text, fyne.TextAlignLeading, s)
	}
	return container.NewGridWithColumns(5,
		cell(address, true), cell(name, false), cell(state, false),
		cell(ping, false), cell(bytes, false))
}

func stateWord(connected bool) string {
	if connected {
		return "online"
	}
	return "connecting"
}

func pingText(ms float64) string {
	if ms <= 0 {
		return "—"
	}
	return fmt.Sprintf("%.0f ms", ms)
}

// Plain words rather than arrows: not every font has the up/down glyphs.
func traffic(rx, tx uint64) string {
	return fmt.Sprintf("%s in · %s out", humanBytes(rx), humanBytes(tx))
}

func plural(n int) string {
	if n == 1 {
		return ""
	}
	return "s"
}

func (u *ui) createDialog() {
	name := widget.NewEntry()
	name.SetPlaceHolder("game night")
	password := widget.NewEntry()
	password.SetPlaceHolder("friends will need this")

	form := dialog.NewForm("Create a network", "Create", "Cancel",
		[]*widget.FormItem{
			widget.NewFormItem("Name", name),
			widget.NewFormItem("Password", password),
		},
		func(ok bool) {
			if !ok {
				return
			}
			if strings.TrimSpace(password.Text) == "" {
				u.notice.SetText("A password is required.")
				return
			}
			label := strings.TrimSpace(name.Text)
			if label == "" {
				label = "my network"
			}
			u.send(client.Request{Command: "create", Name: label, Password: password.Text})
		}, u.window)
	form.Resize(fyne.NewSize(420, 220))
	form.Show()
}

func (u *ui) joinDialog() {
	id := widget.NewEntry()
	id.SetPlaceHolder("HAM-4821-9903")
	password := widget.NewEntry()

	form := dialog.NewForm("Join a network", "Join", "Cancel",
		[]*widget.FormItem{
			widget.NewFormItem("Network ID", id),
			widget.NewFormItem("Password", password),
		},
		func(ok bool) {
			if !ok {
				return
			}
			u.send(client.Request{
				Command:   "join",
				NetworkID: strings.ToUpper(strings.TrimSpace(id.Text)),
				Password:  password.Text,
			})
		}, u.window)
	form.Resize(fyne.NewSize(420, 220))
	form.Show()
}

func (u *ui) confirmDelete(id, name string) {
	dialog.ShowCustomConfirm("Delete this network?", "Delete it", "Cancel",
		widget.NewLabel(fmt.Sprintf(
			"'%s' (%s)\n\nThis removes it for everyone and the ID stops working.\n"+
				"It cannot be undone, and only whoever created it can do this.\n\n"+
				"To stop using it yourself without affecting anyone else, choose Leave.",
			name, id)),
		func(ok bool) {
			if ok {
				u.send(client.Request{Command: "delete", NetworkID: id})
			}
		}, u.window)
}

// send runs a command off the UI thread, so a slow coordinator cannot freeze the
// window, and reports the outcome when it comes back.
func (u *ui) send(req client.Request) {
	b := u.current()
	if b == nil {
		return
	}
	u.notice.SetText("Working…")
	go func() {
		resp := b.Do(req)
		message := resp.Message
		if message == "" && resp.OK {
			message = "Done."
		}
		fyne.Do(func() { u.notice.SetText(strings.ReplaceAll(message, "\n", "  ")) })
	}()
}

func humanBytes(n uint64) string {
	units := []string{"B", "KB", "MB", "GB", "TB"}
	value := float64(n)
	unit := 0
	for value >= 1024 && unit < len(units)-1 {
		value /= 1024
		unit++
	}
	if unit == 0 {
		return fmt.Sprintf("%d B", n)
	}
	return fmt.Sprintf("%.1f %s", value, units[unit])
}

func defaultName() string {
	for _, key := range []string{"SUDO_USER", "USER", "USERNAME", "HOSTNAME", "COMPUTERNAME"} {
		if v := os.Getenv(key); v != "" {
			return v
		}
	}
	if host, err := os.Hostname(); err == nil && host != "" {
		return host
	}
	return "player"
}
