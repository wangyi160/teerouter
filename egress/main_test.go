package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"math/big"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/xtls/xray-core/core"
	_ "github.com/xtls/xray-core/proxy/freedom"
	_ "github.com/xtls/xray-core/proxy/vless/inbound"
	"golang.org/x/net/proxy"
)

const fixtureUUID = "12345678-1234-1234-1234-123456789abc"

func fixtureNode(t *testing.T, network string) (int, []string) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	cert := &x509.Certificate{SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "node.test"}, DNSNames: []string{"node.test"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour), IsCA: true, BasicConstraintsValid: true,
		KeyUsage: x509.KeyUsageCertSign | x509.KeyUsageDigitalSignature, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}}
	der, err := x509.CreateCertificate(rand.Reader, cert, cert, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	rawKey, err := x509.MarshalECPrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	certLines := strings.Split(strings.TrimSpace(string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}))), "\n")
	keyLines := strings.Split(strings.TrimSpace(string(pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: rawKey}))), "\n")
	reserve, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := reserve.Addr().(*net.TCPAddr).Port
	reserve.Close()
	stream := map[string]any{"network": network, "security": "tls", "tlsSettings": map[string]any{"alpn": []string{"h2", "http/1.1"}, "certificates": []any{map[string]any{"certificate": certLines, "key": keyLines}}}}
	if network == "xhttp" {
		stream["xhttpSettings"] = map[string]any{"path": "/fixture", "mode": "auto"}
	}
	config, _ := json.Marshal(map[string]any{"log": map[string]any{"loglevel": "none"}, "inbounds": []any{map[string]any{
		"listen": "127.0.0.1", "port": port, "protocol": "vless", "settings": map[string]any{"clients": []any{map[string]any{"id": fixtureUUID}}, "decryption": "none"}, "streamSettings": stream}},
		"outbounds": []any{map[string]any{"protocol": "freedom"}}})
	instance, err := core.StartInstance("json", config)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { instance.Close() })
	return port, certLines
}

func fixtureOutbound(port int, cert []string, network, mode, uuid, sni string) json.RawMessage {
	stream := map[string]any{"network": network, "security": "tls", "tlsSettings": map[string]any{
		"serverName": sni, "allowInsecure": false, "disableSystemRoot": true, "certificates": []any{map[string]any{"certificate": cert, "usage": "verify"}}}}
	if network == "xhttp" {
		stream["xhttpSettings"] = map[string]any{"path": "/fixture", "mode": mode}
	}
	config, _ := json.Marshal(map[string]any{"log": map[string]any{"loglevel": "none"}, "outbounds": []any{map[string]any{"protocol": "vless", "settings": map[string]any{
		"vnext": []any{map[string]any{"address": "127.0.0.1", "port": port, "users": []any{map[string]any{"id": uuid, "encryption": "none"}}}}}, "streamSettings": stream}}})
	return config
}

func fixtureDialer(t *testing.T) (*dialer, string) {
	t.Helper()
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	d := &dialer{routes: map[string]*route{}, password: "fixture-password", slots: make(chan struct{}, 2048)}
	go func() {
		for {
			c, err := listener.Accept()
			if err != nil {
				return
			}
			d.slots <- struct{}{}
			go d.serve(c)
		}
	}()
	t.Cleanup(func() {
		listener.Close()
		for id := range d.routes {
			d.remove(id)
		}
	})
	return d, listener.Addr().String()
}

func fixtureClient(t *testing.T, address, id, password string) *http.Client {
	t.Helper()
	p, err := proxy.SOCKS5("tcp", address, &proxy.Auth{User: id, Password: password}, proxy.Direct)
	if err != nil {
		t.Fatal(err)
	}
	transport := &http.Transport{DialContext: p.(proxy.ContextDialer).DialContext, DisableKeepAlives: true}
	t.Cleanup(transport.CloseIdleConnections)
	return &http.Client{Transport: transport, Timeout: 3 * time.Second}
}

func TestTLSVLESSRoutesAndFailures(t *testing.T) {
	for _, transport := range []struct{ network, mode string }{{"tcp", ""}, {"xhttp", "stream-one"}, {"xhttp", "stream-up"}, {"xhttp", "packet-up"}} {
		t.Run(transport.network+transport.mode, func(t *testing.T) {
			port, cert := fixtureNode(t, transport.network)
			d, address := fixtureDialer(t)
			var hits atomic.Int32
			target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { hits.Add(1); fmt.Fprint(w, "through-vless") }))
			defer target.Close()
			good := strings.Repeat("1", 64)
			if !d.add(good, fixtureOutbound(port, cert, transport.network, transport.mode, fixtureUUID, "node.test")) {
				t.Fatal("add")
			}
			client := fixtureClient(t, address, good, d.password)
			response, err := client.Get(target.URL)
			if err != nil {
				t.Fatal(err)
			}
			data, err := io.ReadAll(response.Body)
			response.Body.Close()
			if err != nil || string(data) != "through-vless" {
				t.Fatal("response", err)
			}
			if hits.Load() != 1 {
				t.Fatal("upstream hits")
			}
			for i, variant := range []struct{ uuid, sni string }{{fixtureUUID, "wrong.test"}, {"00000000-0000-0000-0000-000000000000", "node.test"}} {
				id := strings.Repeat(fmt.Sprint(i+2), 64)
				if !d.add(id, fixtureOutbound(port, cert, transport.network, transport.mode, variant.uuid, variant.sni)) {
					t.Fatal("add failure fixture")
				}
				c := fixtureClient(t, address, id, d.password)
				if res, err := c.Get(target.URL); err == nil {
					res.Body.Close()
					t.Fatal("invalid TLS/UUID succeeded")
				}
				d.remove(id)
			}
			for _, auth := range []struct{ id, password string }{{good, "wrong"}, {strings.Repeat("f", 64), d.password}} {
				c := fixtureClient(t, address, auth.id, auth.password)
				if res, err := c.Get(target.URL); err == nil {
					res.Body.Close()
					t.Fatal("unauthorized route succeeded")
				}
			}
			if hits.Load() != 1 {
				t.Fatal("failed proxy connected directly")
			}
			// A rejected route/admission must not disrupt an existing account's route.
			d.mu.Lock()
			savedCount := d.instances
			d.instances = maxRoutes
			d.mu.Unlock()
			if d.add(strings.Repeat("e", 64), fixtureOutbound(port, cert, transport.network, transport.mode, fixtureUUID, "node.test")) {
				t.Fatal("capacity exceeded")
			}
			d.mu.Lock()
			d.instances = savedCount
			d.mu.Unlock()
			response, err = client.Get(target.URL)
			if err != nil {
				t.Fatal("existing route disrupted", err)
			}
			response.Body.Close()
			d.remove(good)
			if res, err := client.Get(target.URL); err == nil {
				res.Body.Close()
				t.Fatal("retired route admitted new connection")
			}
		})
	}
}

func TestRetiredRouteDrainsStreamAndCancellationReleasesIt(t *testing.T) {
	port, cert := fixtureNode(t, "xhttp")
	d, address := fixtureDialer(t)
	release := make(chan struct{})
	defer close(release)
	resume := make(chan struct{})
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, "first\n")
		w.(http.Flusher).Flush()
		select {
		case <-resume:
		case <-r.Context().Done():
			return
		}
		fmt.Fprint(w, "second\n")
		w.(http.Flusher).Flush()
		select {
		case <-release:
		case <-r.Context().Done():
		}
	}))
	defer target.Close()
	id := strings.Repeat("a", 64)
	if !d.add(id, fixtureOutbound(port, cert, "xhttp", "stream-one", fixtureUUID, "node.test")) {
		t.Fatal("add")
	}
	client := fixtureClient(t, address, id, d.password)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	request, _ := http.NewRequestWithContext(ctx, "GET", target.URL, nil)
	response, err := client.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	first := make([]byte, 6)
	if _, err := io.ReadFull(response.Body, first); err != nil || string(first) != "first\n" {
		t.Fatal("stream first chunk", err)
	}
	d.remove(id)
	d.mu.Lock()
	count := d.instances
	d.mu.Unlock()
	if count != 1 {
		t.Fatal("active stream closed during remove")
	}
	close(resume)
	second := make([]byte, 7)
	if _, err := io.ReadFull(response.Body, second); err != nil || string(second) != "second\n" {
		t.Fatal("retired stream did not drain", err)
	}
	cancel()
	response.Body.Close()
	until := time.Now().Add(3 * time.Second)
	for {
		d.mu.Lock()
		count = d.instances
		d.mu.Unlock()
		if count == 0 {
			break
		}
		if time.Now().After(until) {
			t.Fatal("cancel leaked route")
		}
		time.Sleep(10 * time.Millisecond)
	}
}
