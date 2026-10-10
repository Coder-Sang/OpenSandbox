// Copyright 2026 The OpenSandbox Authors
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

package controller

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/http/httputil"
	"net/url"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"

	"github.com/alibaba/opensandbox/execd/pkg/jupyter/execute"
	"github.com/alibaba/opensandbox/execd/pkg/runtime"
	"github.com/alibaba/opensandbox/execd/pkg/web/model"
)

func awaitSSELifetime(t *testing.T, signal <-chan struct{}, name string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(5 * time.Second):
		t.Fatalf("timed out waiting for %s", name)
	}
}

// lifetimeParentContext makes the handler's child-context cancellation
// observable through the public context.AfterFunc protocol. Value intentionally
// hides the embedded cancelCtx identity so WithCancel uses that protocol rather
// than registering directly with the embedded request context.
type lifetimeParentContext struct {
	context.Context
	canceled chan struct{}
	once     sync.Once
}

func (c *lifetimeParentContext) Value(any) any { return nil }

func (c *lifetimeParentContext) AfterFunc(f func()) func() bool {
	stop := context.AfterFunc(c.Context, f)
	return func() bool {
		c.once.Do(func() { close(c.canceled) })
		return stop()
	}
}

type lifetimePingGate struct {
	started  chan struct{}
	release  chan struct{}
	blocked  atomic.Bool
	released atomic.Bool
}

type lifetimeWriter struct {
	gin.ResponseWriter
	gate    *lifetimePingGate
	overlap chan struct{}
	once    sync.Once
}

func (w *lifetimeWriter) Write(p []byte) (int, error) {
	if w.gate.blocked.Load() && !w.gate.released.Load() {
		w.once.Do(func() { close(w.overlap) })
	}
	return w.ResponseWriter.Write(p)
}

type lifetimePingConn struct {
	net.Conn
	gate *lifetimePingGate
}

func (c *lifetimePingConn) Write(p []byte) (int, error) {
	if bytes.Contains(p, []byte(`"type":"ping"`)) && c.gate.blocked.CompareAndSwap(false, true) {
		// Delay a genuine net/http socket write without injecting errors or
		// closing the connection. Copying preserves its original payload.
		payload := bytes.Clone(p)
		close(c.gate.started)
		select {
		case <-c.gate.release:
		case <-time.After(5 * time.Second):
			return 0, errors.New("ping gate timed out")
		}
		return c.Conn.Write(payload)
	}
	return c.Conn.Write(p)
}

type lifetimePingListener struct {
	net.Listener
	gate *lifetimePingGate
}

func (l *lifetimePingListener) Accept() (net.Conn, error) {
	conn, err := l.Listener.Accept()
	if err != nil {
		return nil, err
	}
	return &lifetimePingConn{Conn: conn, gate: l.gate}, nil
}

func decodeLifetimeEvents(t *testing.T, body []byte) []model.ServerStreamEvent {
	t.Helper()
	var events []model.ServerStreamEvent
	for _, payload := range bytes.Split(bytes.TrimSpace(body), []byte("\n\n")) {
		var event model.ServerStreamEvent
		require.NoError(t, json.Unmarshal(payload, &event), "every body frame must be an SSE event: %q", payload)
		require.NotEmpty(t, event.Type, "JSON error responses must not be appended to an SSE body")
		events = append(events, event)
	}
	return events
}

// TestRunCodeErrorWaitsForPingHTTP uses a real HTTP/1.1 writer and an optional
// ReverseProxy to verify wire framing, not merely runtime/Jupyter event errors.
// The socket gate forces ping to be in progress when Execute returns an error.
// A cancellation handshake proves the handler reached cleanup before the gate
// is released. No scheduling sleeps or injected connection failures are used.
func TestRunCodeErrorWaitsForPingHTTP(t *testing.T) {
	for _, proxy := range []bool{false, true} {
		for _, large := range []bool{false, true} {
			name := "direct/short"
			if proxy {
				name = "proxy/short"
			}
			if large {
				name = strings.TrimSuffix(name, "short") + "large"
			}
			t.Run(name, func(t *testing.T) {
				message := "session is busy"
				if large {
					// Synthetic size crosses net/http's buffer boundaries. This
					// is a framing regression, not a historical EOF reproducer.
					message += ": " + strings.Repeat("x", 8192)
				}
				gate := &lifetimePingGate{started: make(chan struct{}), release: make(chan struct{})}
				var releaseOnce sync.Once
				release := func() {
					releaseOnce.Do(func() {
						gate.released.Store(true)
						close(gate.release)
					})
				}
				defer release()
				overlap := make(chan struct{})
				canceled := make(chan struct{})
				handlerDone := make(chan struct{})
				previousRunner := codeRunner
				codeRunner = &fakeCodeRunner{execute: func(req *runtime.ExecuteCodeRequest) error {
					req.Hooks.OnExecuteInit("session-1")
					select {
					case <-gate.started:
						return errors.New(message)
					case <-time.After(5 * time.Second):
						return errors.New("ping did not start")
					}
				}}
				defer func() { codeRunner = previousRunner }()

				engine := gin.New()
				engine.POST("/code", func(ctx *gin.Context) {
					parent := &lifetimeParentContext{Context: ctx.Request.Context(), canceled: canceled}
					ctx.Request = ctx.Request.WithContext(parent)
					ctx.Writer = &lifetimeWriter{ResponseWriter: ctx.Writer, gate: gate, overlap: overlap}
					defer close(handlerDone)
					NewCodeInterpretingController(ctx).RunCode()
				})
				server := httptest.NewUnstartedServer(engine)
				server.Listener = &lifetimePingListener{Listener: server.Listener, gate: gate}
				server.Start()
				defer func() { release(); server.Close() }()
				endpoint := server.URL
				if proxy {
					upstream, err := url.Parse(server.URL)
					require.NoError(t, err)
					gateway := httptest.NewServer(httputil.NewSingleHostReverseProxy(upstream))
					defer func() { release(); gateway.Close() }()
					endpoint = gateway.URL
				}
				transport := &http.Transport{DisableKeepAlives: true}
				defer transport.CloseIdleConnections()
				client := &http.Client{Transport: transport, Timeout: 5 * time.Second}
				type result struct {
					response *http.Response
					body     []byte
					err      error
				}
				resultCh := make(chan result, 1)
				go func() {
					response, err := client.Post(endpoint+"/code", "application/json", strings.NewReader(`{"code":"print(1)","context":{"id":"session-1","language":"python"}}`))
					if err != nil {
						resultCh <- result{err: err}
						return
					}
					defer response.Body.Close()
					body, err := io.ReadAll(response.Body)
					resultCh <- result{response: response, body: body, err: err}
				}()

				awaitSSELifetime(t, gate.started, "ping socket write")
				select {
				case <-canceled:
				case <-overlap:
					t.Fatal("error response was written while ping was still using the response writer")
				case <-time.After(5 * time.Second):
					t.Fatal("handler did not cancel ping after Execute returned an error")
				}
				select {
				case <-overlap:
					t.Fatal("error response overlapped the blocked ping")
				case <-handlerDone:
					t.Fatal("handler returned before blocked ping stopped")
				default:
				}
				release()
				awaitSSELifetime(t, handlerDone, "handler cleanup")
				select {
				case <-overlap:
					t.Fatal("response write occurred before the blocked ping was released")
				default:
				}
				var got result
				select {
				case got = <-resultCh:
				case <-time.After(5 * time.Second):
					t.Fatal("HTTP client did not finish")
				}
				require.NoError(t, got.err, "chunked HTTP body must terminate cleanly")
				require.NotNil(t, got.response)
				require.Equal(t, http.StatusOK, got.response.StatusCode)
				require.Equal(t, []string{"chunked"}, got.response.TransferEncoding)
				require.Contains(t, got.response.Header.Get("Content-Type"), "text/event-stream")
				events := decodeLifetimeEvents(t, got.body)
				require.Equal(t, model.StreamEventTypeInit, events[0].Type)
				require.Equal(t, model.StreamEventTypeError, events[len(events)-1].Type)
				var errorsSeen int
				for _, event := range events {
					if event.Type == model.StreamEventTypeError {
						errorsSeen++
						require.NotNil(t, event.Error)
						require.Equal(t, "RuntimeError", event.Error.EName)
						require.Contains(t, event.Error.EValue, message)
					}
				}
				require.Equal(t, 1, errorsSeen)
			})
		}
	}
}

func TestRunCodeReturnedErrorPreservesTerminalEvent(t *testing.T) {
	previousRunner := codeRunner
	defer func() { codeRunner = previousRunner }()
	codeRunner = &fakeCodeRunner{execute: func(req *runtime.ExecuteCodeRequest) error {
		req.Hooks.OnExecuteInit("session-1")
		req.Hooks.OnExecuteError(&execute.ErrorOutput{EName: "ContextCancelled", EValue: "Interrupt kernel"})
		return errors.New("context cancelled, interrupt kernel")
	}}
	ctx, recorder := newTestContext(http.MethodPost, "/code", []byte(`{"code":"print(1)","context":{"id":"session-1","language":"python"}}`))
	NewCodeInterpretingController(ctx).RunCode()
	events := decodeLifetimeEvents(t, recorder.Body.Bytes())
	var terminalErrors int
	for _, event := range events {
		if event.Type == model.StreamEventTypeError {
			terminalErrors++
			require.NotNil(t, event.Error)
			require.Equal(t, "ContextCancelled", event.Error.EName)
		}
	}
	require.Equal(t, 1, terminalErrors, "Execute's returned error must not duplicate its terminal hook")
	require.Equal(t, http.StatusOK, recorder.Code)
}

func TestRunCodeReturnedErrorBeforeInitPreservesJSON(t *testing.T) {
	previousRunner := codeRunner
	defer func() { codeRunner = previousRunner }()
	codeRunner = &fakeCodeRunner{execute: func(*runtime.ExecuteCodeRequest) error {
		return errors.New("synchronous runtime failure")
	}}
	ctx, recorder := newTestContext(http.MethodPost, "/code", []byte(`{"code":"print(1)","context":{"id":"session-1","language":"python"}}`))
	NewCodeInterpretingController(ctx).RunCode()
	require.Equal(t, http.StatusInternalServerError, recorder.Code)
	require.Contains(t, recorder.Header().Get("Content-Type"), "application/json")
	var response model.ErrorResponse
	require.NoError(t, json.Unmarshal(recorder.Body.Bytes(), &response))
	require.Equal(t, model.ErrorCodeRuntimeError, response.Code)
	require.Contains(t, response.Message, "synchronous runtime failure")
}
