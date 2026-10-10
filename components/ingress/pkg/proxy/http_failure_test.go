// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

package proxy

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	slogger "github.com/alibaba/opensandbox/internal/logger"
)

type failureLogEntry struct {
	message string
	fields  map[string]any
}
type failureLogSink struct {
	mu      sync.Mutex
	entries []failureLogEntry
	logged  chan struct{}
}
type failureTestLogger struct {
	sink   *failureLogSink
	fields []slogger.Field
}

func (l *failureTestLogger) Debugf(string, ...any) {}
func (l *failureTestLogger) Infof(string, ...any)  {}
func (l *failureTestLogger) Warnf(string, ...any)  {}
func (l *failureTestLogger) Errorf(format string, args ...any) {
	fields := map[string]any{}
	for _, f := range l.fields {
		fields[f.Key] = f.Value
	}
	l.sink.mu.Lock()
	l.sink.entries = append(l.sink.entries, failureLogEntry{fmt.Sprintf(format, args...), fields})
	l.sink.mu.Unlock()
	if l.sink.logged != nil {
		select {
		case l.sink.logged <- struct{}{}:
		default:
		}
	}
}
func (l *failureTestLogger) With(fields ...slogger.Field) slogger.Logger {
	f := append([]slogger.Field{}, l.fields...)
	return &failureTestLogger{l.sink, append(f, fields...)}
}
func (l *failureTestLogger) Named(string) slogger.Logger { return l }
func (l *failureTestLogger) Sync() error                 { return nil }
func captureFailureLogs(t *testing.T) *failureLogSink {
	t.Helper()
	sink := &failureLogSink{logged: make(chan struct{}, 8)}
	old := Logger
	Logger = &failureTestLogger{sink: sink}
	t.Cleanup(func() { Logger = old })
	return sink
}
func (s *failureLogSink) snapshot() []failureLogEntry {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]failureLogEntry{}, s.entries...)
}
func failureWait(t *testing.T, ch <-chan struct{}) {
	t.Helper()
	select {
	case <-ch:
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for proxy failure")
	}
}
func failureFrontend(t *testing.T, back *httptest.Server, mutate func(*http.Request)) (*httptest.Server, *http.Transport) {
	t.Helper()
	target, _ := url.Parse(back.URL)
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Proxy = nil
	hp := NewHTTPProxy()
	hp.transport = transport
	front := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		r.URL.Scheme = target.Scheme
		r.URL.Host = target.Host
		if mutate != nil {
			mutate(r)
		}
		hp.ServeHTTP(&statusCapturingResponseWriter{ResponseWriter: w, statusCode: http.StatusOK}, r)
	}))
	t.Cleanup(func() { front.Close(); transport.CloseIdleConnections() })
	return front, transport
}

const failureEvent = "data: {\"type\":\"execution_complete\"}\n\n"

func readFailureEvent(t *testing.T, res *http.Response) {
	t.Helper()
	b := make([]byte, len(failureEvent))
	_, err := io.ReadFull(res.Body, b)
	if err != nil || string(b) != failureEvent {
		t.Fatalf("event=%q err=%v", b, err)
	}
}
func checkFailureFields(t *testing.T, sink *failureLogSink, errorClass, contextClass, writeClass string) {
	t.Helper()
	entries := sink.snapshot()
	if len(entries) != 1 {
		t.Fatalf("entries=%+v", entries)
	}
	f := entries[0].fields
	want := map[string]any{"method": "POST", "path": "/code", "failure_phase": "response_copy", "error_class": errorClass, "context_error": contextClass, "got_conn": true, "reused": false, "request_write_error": writeClass}
	for k, v := range want {
		if f[k] != v {
			t.Errorf("%s=%v want=%v", k, f[k], v)
		}
	}
	for _, k := range []string{"upstream_local", "upstream_remote"} {
		if _, _, err := net.SplitHostPort(f[k].(string)); err != nil {
			t.Errorf("invalid tuple field %s=%v", k, f[k])
		}
	}
	wire := fmt.Sprint(entries)
	for _, secret := range []string{"query-secret", "header-secret", "body-secret", "error-secret"} {
		if strings.Contains(wire, secret) {
			t.Errorf("leaked %s", secret)
		}
	}
}
func failureRequest(t *testing.T, front *httptest.Server) *http.Response {
	t.Helper()
	req, _ := http.NewRequest("POST", front.URL+"/code?token=query-secret", strings.NewReader("body-secret"))
	req.Header.Set("Authorization", "header-secret")
	res, err := front.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	return res
}
func TestHTTPProxyFailureDiagnostics(t *testing.T) {
	t.Run("upstream truncation", func(t *testing.T) {
		sink := captureFailureLogs(t)
		release := make(chan struct{})
		back := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			io.ReadAll(r.Body)
			c, b, err := w.(http.Hijacker).Hijack()
			if err != nil {
				t.Error(err)
				return
			}
			defer c.Close()
			fmt.Fprintf(b, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n%x\r\n%s\r\n", len(failureEvent), failureEvent)
			b.Flush()
			<-release
		}))
		defer back.Close()
		front, _ := failureFrontend(t, back, nil)
		res := failureRequest(t, front)
		readFailureEvent(t, res)
		close(release)
		_, err := io.ReadAll(res.Body)
		res.Body.Close()
		if !errors.Is(err, io.ErrUnexpectedEOF) {
			t.Fatalf("body error=%v", err)
		}
		failureWait(t, sink.logged)
		checkFailureFields(t, sink, "unexpected_eof", "none", "none")
	})
	t.Run("downstream cancellation", func(t *testing.T) {
		sink := captureFailureLogs(t)
		canceled := make(chan struct{})
		back := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			io.ReadAll(r.Body)
			w.Header().Set("Content-Type", "text/event-stream")
			io.WriteString(w, failureEvent)
			w.(http.Flusher).Flush()
			<-r.Context().Done()
			close(canceled)
		}))
		defer back.Close()
		front, _ := failureFrontend(t, back, nil)
		res := failureRequest(t, front)
		readFailureEvent(t, res)
		res.Body.Close()
		failureWait(t, canceled)
		failureWait(t, sink.logged)
		checkFailureFields(t, sink, "canceled", "canceled", "none")
	})
	t.Run("outbound body read failure after response", func(t *testing.T) {
		sink := captureFailureLogs(t)
		release := make(chan struct{})
		back := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			c, b, err := w.(http.Hijacker).Hijack()
			if err != nil {
				t.Error(err)
				return
			}
			defer c.Close()
			fmt.Fprintf(b, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n%x\r\n%s\r\n", len(failureEvent), failureEvent)
			b.Flush()
			io.Copy(io.Discard, b)
		}))
		defer back.Close()
		front, _ := failureFrontend(t, back, func(r *http.Request) { r.Body = &failureRequestBody{release: release}; r.ContentLength = -1 })
		res := failureRequest(t, front)
		readFailureEvent(t, res)
		close(release)
		_, err := io.ReadAll(res.Body)
		res.Body.Close()
		if !errors.Is(err, io.ErrUnexpectedEOF) {
			t.Fatalf("body error=%v", err)
		}
		failureWait(t, sink.logged)
		checkFailureFields(t, sink, "closed", "none", "other")
	})
}

type failureRequestBody struct {
	sent    bool
	release <-chan struct{}
}

func (b *failureRequestBody) Read(p []byte) (int, error) {
	if !b.sent {
		b.sent = true
		return copy(p, "{"), nil
	}
	<-b.release
	return 0, errors.New("error-secret https://user:password@example.test/?token=query-secret")
}
func (*failureRequestBody) Close() error { return nil }
func TestHTTPProxyDiagnosticsSuccessfulReuseSilent(t *testing.T) {
	sink := captureFailureLogs(t)
	observed := 0
	back := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { io.ReadAll(r.Body); io.WriteString(w, "ok") }))
	defer back.Close()
	target, _ := url.Parse(back.URL)
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Proxy = nil
	defer transport.CloseIdleConnections()
	hp := NewHTTPProxy(func(*http.Response) { observed++ })
	hp.transport = transport
	front := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		r.URL.Scheme = target.Scheme
		r.URL.Host = target.Host
		hp.ServeHTTP(&statusCapturingResponseWriter{ResponseWriter: w, statusCode: http.StatusOK}, r)
	}))
	defer front.Close()
	for i := 0; i < 2; i++ {
		res := failureRequest(t, front)
		b, err := io.ReadAll(res.Body)
		res.Body.Close()
		if err != nil || string(b) != "ok" || res.Header.Get(ReverseProxyServerPowerBy) != "OpenSandbox-ingress" {
			t.Fatalf("body=%q err=%v headers=%v", b, err, res.Header)
		}
	}
	if observed != 2 || len(sink.snapshot()) != 0 {
		t.Fatalf("observer=%d logs=%v", observed, sink.snapshot())
	}
}

type diagnosticRoundTripper func(*http.Request) (*http.Response, error)

func (f diagnosticRoundTripper) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

func TestHTTPProxyDiagnosticsPreheaderFailure(t *testing.T) {
	sink := captureFailureLogs(t)
	original := &url.Error{Op: "POST", URL: "https://user:password@host/?token=query-secret", Err: errors.New("error-secret")}
	var observed error
	hp := NewHTTPProxy()
	hp.transport = diagnosticRoundTripper(func(*http.Request) (*http.Response, error) { return nil, original })
	hp.errorObserver = func(err error) { observed = err }
	r := httptest.NewRequest("POST", "http://host/code?token=query-secret", strings.NewReader("body-secret"))
	r.Header.Set("Authorization", "header-secret")
	w := httptest.NewRecorder()
	hp.ServeHTTP(w, r)
	entries := sink.snapshot()
	if w.Code != http.StatusBadGateway || observed != original || len(entries) != 1 {
		t.Fatalf("status=%d observer=%v entries=%v", w.Code, observed, entries)
	}
	f := entries[0].fields
	if f["failure_phase"] != "round_trip" || f["got_conn"] != false || f["request_write_error"] != "not_observed" || f["error_class"] != "other" || strings.Contains(fmt.Sprint(entries), "secret") {
		t.Fatalf("unsafe or incorrect fields=%v", f)
	}
}

func TestHTTPProxyDiagnosticsReusedConnection(t *testing.T) {
	sink := captureFailureLogs(t)
	back := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.ReadAll(r.Body)
		if r.URL.Path == "/warm" {
			io.WriteString(w, "ok")
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Content-Length", "100")
		io.WriteString(w, failureEvent)
	}))
	defer back.Close()
	front, _ := failureFrontend(t, back, nil)
	warm, err := front.Client().Post(front.URL+"/warm", "application/json", strings.NewReader("{}"))
	if err != nil {
		t.Fatal(err)
	}
	io.Copy(io.Discard, warm.Body)
	warm.Body.Close()
	res := failureRequest(t, front)
	_, err = io.ReadAll(res.Body)
	res.Body.Close()
	if !errors.Is(err, io.ErrUnexpectedEOF) {
		t.Fatalf("body error=%v", err)
	}
	failureWait(t, sink.logged)
	entries := sink.snapshot()
	if len(entries) != 1 || entries[0].fields["reused"] != true || entries[0].fields["error_class"] != "unexpected_eof" {
		t.Fatalf("entries=%v", entries)
	}
}

type diagnosticUpgradeBody struct{ io.Reader }

func (*diagnosticUpgradeBody) Write(p []byte) (int, error) { return len(p), nil }
func (*diagnosticUpgradeBody) Close() error                { return nil }

func TestHTTPProxyDiagnosticsPreserveUpgradeBody(t *testing.T) {
	sink := captureFailureLogs(t)
	r := httptest.NewRequest("GET", "http://host/code", nil)
	d := &proxyFailureDiagnostics{request: r, logger: Logger}
	hp := NewHTTPProxy()
	rp := hp.newReverseProxy(r.URL, d)
	body := &diagnosticUpgradeBody{Reader: strings.NewReader("")}
	res := &http.Response{StatusCode: http.StatusSwitchingProtocols, Header: make(http.Header), Body: body}
	if err := rp.ModifyResponse(res); err != nil || res.Body != body {
		t.Fatalf("upgrade body changed: %T err=%v", res.Body, err)
	}
	if _, ok := res.Body.(io.ReadWriteCloser); !ok {
		t.Fatal("upgrade body no longer writable")
	}
	if len(sink.snapshot()) != 0 {
		t.Fatal("successful upgrade emitted diagnostics")
	}
}

func TestHTTPProxyDiagnosticsUpgradeWire(t *testing.T) {
	sink := captureFailureLogs(t)
	backendDone := make(chan struct{})
	back := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, rw, err := w.(http.Hijacker).Hijack()
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.Close()
		defer close(backendDone)
		fmt.Fprint(rw, "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: diagnostic\r\n\r\n")
		rw.Flush()
		b := make([]byte, 3)
		if _, err := io.ReadFull(rw, b); err != nil {
			t.Error(err)
			return
		}
		rw.Write(b)
		rw.Flush()
	}))
	defer back.Close()
	front, _ := failureFrontend(t, back, nil)
	conn, err := net.Dial("tcp", front.Listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	conn.SetDeadline(time.Now().Add(5 * time.Second))
	fmt.Fprint(conn, "GET /code HTTP/1.1\r\nHost: diagnostic\r\nConnection: Upgrade\r\nUpgrade: diagnostic\r\n\r\n")
	reader := bufio.NewReader(conn)
	res, err := http.ReadResponse(reader, nil)
	if err != nil || res.StatusCode != http.StatusSwitchingProtocols {
		t.Fatalf("response=%v error=%v", res, err)
	}
	fmt.Fprint(conn, "abc")
	b := make([]byte, 3)
	if _, err := io.ReadFull(reader, b); err != nil || string(b) != "abc" {
		t.Fatalf("echo=%q err=%v", b, err)
	}
	failureWait(t, backendDone)
	if len(sink.snapshot()) != 0 {
		t.Fatalf("upgrade logs=%v", sink.snapshot())
	}
}

func TestProxyErrorClassPrivacyAndWrapping(t *testing.T) {
	for _, test := range []struct {
		err  error
		want string
	}{{nil, "none"}, {context.Canceled, "canceled"}, {context.DeadlineExceeded, "deadline_exceeded"}, {io.EOF, "eof"}, {io.ErrUnexpectedEOF, "unexpected_eof"}, {net.ErrClosed, "closed"}, {syscall.ECONNRESET, "connection_reset"}, {syscall.ECONNREFUSED, "connection_refused"}, {syscall.EPIPE, "broken_pipe"}, {&net.DNSError{IsTimeout: true}, "timeout"}, {errors.New("error-secret"), "other"}} {
		if got := proxyErrorClass(test.err); got != test.want {
			t.Errorf("got=%s want=%s", got, test.want)
		}
		if test.err != nil {
			wrapped := &url.Error{Op: "POST", URL: "https://secret:password@host/?token=query-secret", Err: test.err}
			if got := proxyErrorClass(wrapped); got != test.want {
				t.Errorf("wrapped got=%s want=%s", got, test.want)
			}
		}
	}
	sink := captureFailureLogs(t)
	r := httptest.NewRequest("POST", "http://host/code?token=query-secret", strings.NewReader("body-secret"))
	d := &proxyFailureDiagnostics{request: r, logger: Logger, requestWriteError: "not_observed"}
	n, err := d.Write([]byte("unknown error-secret header-secret body-secret query-secret\n"))
	if err != nil || n == 0 {
		t.Fatal(err)
	}
	e := sink.snapshot()
	if len(e) != 1 || e[0].fields["failure_phase"] != "reverse_proxy" || e[0].fields["got_conn"] != false || strings.Contains(fmt.Sprint(e), "secret") {
		t.Fatalf("unsafe or incorrect unknown log: %+v", e)
	}
}
