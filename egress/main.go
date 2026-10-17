// Private, memory-only VLESS dialer for teerouter. No public management API.
package main

import (
	"bufio"
	"context"
	"crypto/rand"
	"crypto/subtle"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"io"
	"log"
	"net"
	"os"
	"strconv"
	"sync"
	"time"

	_ "github.com/xtls/xray-core/app/dispatcher"
	_ "github.com/xtls/xray-core/app/log"
	_ "github.com/xtls/xray-core/app/proxyman/inbound"
	_ "github.com/xtls/xray-core/app/proxyman/outbound"
	xnet "github.com/xtls/xray-core/common/net"
	"github.com/xtls/xray-core/core"
	_ "github.com/xtls/xray-core/main/json"
	_ "github.com/xtls/xray-core/proxy/vless/outbound"
	_ "github.com/xtls/xray-core/transport/internet/splithttp"
	_ "github.com/xtls/xray-core/transport/internet/tagged/taggedimpl"
	_ "github.com/xtls/xray-core/transport/internet/tcp"
	_ "github.com/xtls/xray-core/transport/internet/tls"
)

const maxRoutes = 64

type route struct {
	instance *core.Instance
	refs     int
	retired  bool
}

type dialer struct {
	mu        sync.Mutex
	routes    map[string]*route
	password  string
	slots     chan struct{}
	instances int // Includes retired routes still draining existing connections.
}

func (d *dialer) add(id string, config json.RawMessage) bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	if len(id) != 64 || d.instances >= maxRoutes || d.routes[id] != nil {
		return false
	}
	if _, err := hex.DecodeString(id); err != nil {
		return false
	}
	instance, err := core.StartInstance("json", config)
	if err != nil {
		return false
	}
	d.routes[id] = &route{instance: instance}
	d.instances++
	return true
}

func (d *dialer) remove(id string) {
	d.mu.Lock()
	r := d.routes[id]
	delete(d.routes, id)
	if r != nil {
		r.retired = true
	}
	closeNow := r != nil && r.refs == 0
	if closeNow {
		d.instances--
	}
	d.mu.Unlock()
	if closeNow {
		_ = r.instance.Close()
	}
}

func (d *dialer) acquire(id, password string) *route {
	d.mu.Lock()
	defer d.mu.Unlock()
	if subtle.ConstantTimeCompare([]byte(password), []byte(d.password)) != 1 {
		return nil
	}
	r := d.routes[id]
	if r != nil {
		r.refs++
	}
	return r
}

func (d *dialer) release(r *route) {
	d.mu.Lock()
	r.refs--
	closeNow := r.retired && r.refs == 0
	if closeNow {
		d.instances--
	}
	d.mu.Unlock()
	if closeNow {
		_ = r.instance.Close()
	}
}

func readBytes(r io.Reader, n int) ([]byte, error) {
	b := make([]byte, n)
	_, err := io.ReadFull(r, b)
	return b, err
}

func shortString(r io.Reader) (string, error) {
	n, err := readBytes(r, 1)
	if err != nil {
		return "", err
	}
	b, err := readBytes(r, int(n[0]))
	return string(b), err
}

func (d *dialer) serve(c net.Conn) {
	defer c.Close()
	defer func() { <-d.slots }()
	_ = c.SetDeadline(time.Now().Add(10 * time.Second))
	header, err := readBytes(c, 2)
	if err != nil || header[0] != 5 || header[1] == 0 {
		return
	}
	methods, err := readBytes(c, int(header[1]))
	if err != nil {
		return
	}
	auth := false
	for _, m := range methods {
		auth = auth || m == 2
	}
	if !auth {
		_, _ = c.Write([]byte{5, 255})
		return
	}
	if _, err = c.Write([]byte{5, 2}); err != nil {
		return
	}
	version, err := readBytes(c, 1)
	if err != nil || version[0] != 1 {
		return
	}
	user, err := shortString(c)
	if err != nil {
		return
	}
	password, err := shortString(c)
	if err != nil {
		return
	}
	r := d.acquire(user, password)
	if r == nil {
		_, _ = c.Write([]byte{1, 1})
		return
	}
	defer d.release(r)
	if _, err = c.Write([]byte{1, 0}); err != nil {
		return
	}
	header, err = readBytes(c, 4)
	if err != nil || header[0] != 5 || header[1] != 1 || header[2] != 0 {
		return
	}
	var host string
	switch header[3] {
	case 1, 4:
		n := 4
		if header[3] == 4 {
			n = 16
		}
		var ip []byte
		ip, err = readBytes(c, n)
		host = net.IP(ip).String()
	case 3:
		host, err = shortString(c)
	default:
		return
	}
	if err != nil || host == "" {
		return
	}
	port, err := readBytes(c, 2)
	if err != nil || binary.BigEndian.Uint16(port) == 0 {
		return
	}
	destination, err := xnet.ParseDestination("tcp:" + net.JoinHostPort(host, strconv.Itoa(int(binary.BigEndian.Uint16(port)))))
	if err != nil {
		return
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	upstream, err := core.Dial(ctx, r.instance, destination)
	if err != nil {
		_, _ = c.Write([]byte{5, 5, 0, 1, 0, 0, 0, 0, 0, 0})
		return
	}
	defer upstream.Close()
	if _, err = c.Write([]byte{5, 0, 0, 1, 127, 0, 0, 1, 0, 0}); err != nil {
		return
	}
	_ = c.SetDeadline(time.Time{})
	done := make(chan struct{})
	go func() {
		_, _ = io.Copy(upstream, c)
		_ = upstream.Close()
		close(done)
	}()
	_, _ = io.Copy(c, upstream)
	_ = c.Close()
	_ = upstream.Close()
	<-done
}

func main() {
	log.SetOutput(io.Discard)
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		os.Exit(1)
	}
	defer listener.Close()
	secret := make([]byte, 32)
	if _, err := rand.Read(secret); err != nil {
		os.Exit(1)
	}
	d := &dialer{routes: make(map[string]*route), password: hex.EncodeToString(secret), slots: make(chan struct{}, 2048)}
	go func() {
		for {
			c, err := listener.Accept()
			if err != nil {
				return
			}
			select {
			case d.slots <- struct{}{}:
				go d.serve(c)
			default:
				_ = c.Close()
			}
		}
	}()
	out := json.NewEncoder(os.Stdout)
	if out.Encode(map[string]any{"ready": true, "version": 1, "port": listener.Addr().(*net.TCPAddr).Port, "password": d.password}) != nil {
		return
	}
	input := bufio.NewScanner(os.Stdin)
	input.Buffer(make([]byte, 4096), 64*1024)
	for input.Scan() {
		var command struct {
			Op     string          `json:"op"`
			ID     string          `json:"id"`
			Config json.RawMessage `json:"config"`
		}
		if json.Unmarshal(input.Bytes(), &command) != nil {
			return
		}
		ok := false
		switch command.Op {
		case "add":
			ok = d.add(command.ID, command.Config)
		case "remove":
			d.remove(command.ID)
			ok = true
		default:
			return
		}
		if out.Encode(map[string]any{"ok": ok}) != nil {
			return
		}
	}
	// Parent exit closes stdin. Returning terminates all routes and connections.
}
