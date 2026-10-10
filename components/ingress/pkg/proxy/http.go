// Copyright 2025 The OpenSandbox Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package proxy

import (
	"context"
	"errors"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httptrace"
	"net/http/httputil"
	"net/url"
	"strings"
	"sync"
	"syscall"

	slogger "github.com/alibaba/opensandbox/internal/logger"
)

type HTTPProxy struct {
	responseObservers []func(*http.Response)
	errorObserver     func(error)
	transport         http.RoundTripper
}

func NewHTTPProxy(observers ...func(*http.Response)) *HTTPProxy {
	return &HTTPProxy{responseObservers: observers}
}

func (hp *HTTPProxy) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	targetURL := *r.URL

	diagnostics := &proxyFailureDiagnostics{request: r, logger: Logger, requestWriteError: "not_observed"}
	trace := &httptrace.ClientTrace{
		GotConn: diagnostics.gotConnection,
		WroteRequest: func(info httptrace.WroteRequestInfo) {
			diagnostics.mu.Lock()
			diagnostics.requestWriteError = proxyErrorClass(info.Err)
			diagnostics.mu.Unlock()
		},
	}
	r = r.WithContext(httptrace.WithClientTrace(r.Context(), trace))
	proxy := hp.newReverseProxy(&targetURL, diagnostics)
	defer func() {
		if value := recover(); value != nil {
			if err, ok := value.(error); ok && errors.Is(err, http.ErrAbortHandler) {
				diagnostics.record("response_copy", "other")
			}
			panic(value)
		}
	}()
	proxy.ServeHTTP(w, r)
}

func (hp *HTTPProxy) newReverseProxy(targetURL *url.URL, diagnostics *proxyFailureDiagnostics) *httputil.ReverseProxy {
	proxy := httputil.NewSingleHostReverseProxy(targetURL)
	proxy.ErrorLog = log.New(diagnostics, "", 0)
	if hp.transport != nil {
		proxy.Transport = hp.transport
	}
	proxy.Director = func(req *http.Request) {
		req.URL.Scheme = targetURL.Scheme
		req.URL.Host = targetURL.Host
		req.URL.Path = targetURL.Path
		req.URL.RawPath = targetURL.RawPath
		req.URL.RawQuery = targetURL.RawQuery
		req.Host = targetURL.Host
		req.Header.Del(SandboxIngress)
	}
	proxy.ModifyResponse = func(response *http.Response) error {
		for _, observe := range hp.responseObservers {
			if observe != nil {
				observe(response)
			}
		}
		response.Header.Add(ReverseProxyServerPowerBy, "OpenSandbox-ingress")
		if response.Body != nil && response.StatusCode != http.StatusSwitchingProtocols {
			response.Body = &diagnosticResponseBody{ReadCloser: response.Body, diagnostics: diagnostics}
		}
		return nil
	}
	// Custom error handler: log the upstream error and return 502 only
	// when the response headers have not yet been committed. If the
	// response is already streaming (e.g. SSE), writing an error body
	// would corrupt the stream, so we silently let the connection close.
	proxy.ErrorHandler = func(rw http.ResponseWriter, req *http.Request, err error) {
		diagnostics.record("round_trip", proxyErrorClass(err))
		if req.Context().Err() == nil && hp.errorObserver != nil {
			hp.errorObserver(err)
		}

		// Attempt to set 502; this is a no-op if headers are already sent.
		rw.WriteHeader(http.StatusBadGateway)
	}
	return proxy
}

// Diagnostics are request-scoped and emitted only on failure. Never forward
// arbitrary standard-library log text: errors can contain URLs or user data.
type proxyFailureDiagnostics struct {
	mu                sync.Mutex
	request           *http.Request
	logger            slogger.Logger
	gotConn           bool
	upstreamLocal     string
	upstreamRemote    string
	reused            bool
	requestWriteError string
	responseReadError string
	logged            bool
}

func (d *proxyFailureDiagnostics) gotConnection(info httptrace.GotConnInfo) {
	d.mu.Lock()
	defer d.mu.Unlock()
	d.gotConn = true
	d.reused = info.Reused
	d.upstreamLocal = info.Conn.LocalAddr().String()
	d.upstreamRemote = info.Conn.RemoteAddr().String()
}

func (d *proxyFailureDiagnostics) Write(p []byte) (int, error) {
	phase := "reverse_proxy"
	if strings.HasPrefix(string(p), "httputil: ReverseProxy read error during body copy:") {
		phase = "response_copy"
	}
	d.record(phase, "other")
	return len(p), nil
}

func (d *proxyFailureDiagnostics) record(phase, errorClass string) {
	d.mu.Lock()
	defer d.mu.Unlock()
	if d.logged {
		return
	}
	d.logged = true
	if phase == "response_copy" && d.responseReadError != "" {
		errorClass = d.responseReadError
	}
	d.logger.With(
		slogger.Field{Key: "method", Value: d.request.Method},
		slogger.Field{Key: "path", Value: d.request.URL.Path},
		slogger.Field{Key: "failure_phase", Value: phase},
		slogger.Field{Key: "error_class", Value: errorClass},
		slogger.Field{Key: "context_error", Value: proxyErrorClass(d.request.Context().Err())},
		slogger.Field{Key: "got_conn", Value: d.gotConn},
		slogger.Field{Key: "upstream_local", Value: d.upstreamLocal},
		slogger.Field{Key: "upstream_remote", Value: d.upstreamRemote},
		slogger.Field{Key: "reused", Value: d.reused},
		slogger.Field{Key: "request_write_error", Value: d.requestWriteError},
	).Errorf("ingress: reverse proxy failure")
}

type diagnosticResponseBody struct {
	io.ReadCloser
	diagnostics *proxyFailureDiagnostics
}

func (b *diagnosticResponseBody) Read(p []byte) (int, error) {
	n, err := b.ReadCloser.Read(p)
	if err != nil && err != io.EOF {
		b.diagnostics.mu.Lock()
		b.diagnostics.responseReadError = proxyErrorClass(err)
		b.diagnostics.mu.Unlock()
	}
	return n, err
}

func proxyErrorClass(err error) string {
	switch {
	case err == nil:
		return "none"
	case errors.Is(err, context.Canceled):
		return "canceled"
	case errors.Is(err, context.DeadlineExceeded):
		return "deadline_exceeded"
	case errors.Is(err, io.ErrUnexpectedEOF):
		return "unexpected_eof"
	case errors.Is(err, io.EOF):
		return "eof"
	case errors.Is(err, net.ErrClosed):
		return "closed"
	case errors.Is(err, syscall.ECONNRESET):
		return "connection_reset"
	case errors.Is(err, syscall.ECONNREFUSED):
		return "connection_refused"
	case errors.Is(err, syscall.EPIPE):
		return "broken_pipe"
	}
	var networkError net.Error
	if errors.As(err, &networkError) && networkError.Timeout() {
		return "timeout"
	}
	return "other"
}
