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

type lifetimeExecutionCase struct {
	name, route, path, body string
	run                     func(*CodeInterpretingController)
}

func lifetimeExecutionCases() []lifetimeExecutionCase {
	return []lifetimeExecutionCase{
		{"RunCode", "/code", "/code", `{"code":"print(1)","context":{"id":"session-1","language":"python"}}`, (*CodeInterpretingController).RunCode},
		{"RunCommand/foreground", "/command", "/command", `{"command":"true","background":false}`, (*CodeInterpretingController).RunCommand},
		{"RunCommand/background", "/command", "/command", `{"command":"true","background":true}`, (*CodeInterpretingController).RunCommand},
		{"RunInSession", "/session/:sessionId/run", "/session/session-1/run", `{"command":"true","timeout":0}`, (*CodeInterpretingController).RunInSession},
	}
}

func installLifetimeRunner(t *testing.T, run func(*runtime.ExecuteCodeRequest) error) {
	t.Helper()
	previous := codeRunner
	codeRunner = &fakeCodeRunner{
		execute:          run,
		runInBashSession: func(_ context.Context, req *runtime.ExecuteCodeRequest) error { return run(req) },
	}
	t.Cleanup(func() { codeRunner = previous })
}

func lifetimeTestContext(tc lifetimeExecutionCase) (*gin.Context, *httptest.ResponseRecorder) {
	ctx, recorder := newTestContext(http.MethodPost, tc.path, []byte(tc.body))
	if tc.name == "RunInSession" {
		ctx.Params = gin.Params{{Key: "sessionId", Value: "session-1"}}
	}
	return ctx, recorder
}

// TestExecutionErrorWaitsForPingHTTP uses a real HTTP/1.1 writer and an optional
// ReverseProxy to verify wire framing, not merely runtime/Jupyter event errors.
// The socket gate forces ping to be in progress when Execute returns an error.
// A cancellation handshake proves the handler reached cleanup before the gate
// is released. No scheduling sleeps or injected connection failures are used.
func TestExecutionErrorWaitsForPingHTTP(t *testing.T) {
	testExecutionWaitsForPingHTTP(t, "returned-error", false)
}

func TestExecutionTerminalHookWaitsForPingHTTP(t *testing.T) {
	for _, terminal := range []string{"error", "complete"} {
		t.Run(terminal, func(t *testing.T) {
			testExecutionWaitsForPingHTTP(t, terminal, false)
		})
	}
}

// The direct setter's parent observes cancellation of the SSE-owned ping
// context itself, rather than cancellation performed by an execution handler.
func TestExecutionTerminalHookCancelsOwnedPingHTTP(t *testing.T) {
	for _, terminal := range []string{"error", "complete"} {
		t.Run(terminal, func(t *testing.T) {
			testExecutionWaitsForPingHTTP(t, terminal, true)
		})
	}
}

func testExecutionWaitsForPingHTTP(t *testing.T, terminal string, directSetter bool) {
	t.Helper()
	cases := lifetimeExecutionCases()
	if directSetter {
		cases = cases[:1]
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			proxyCases, sizeCases := []bool{false, true}, []bool{false, true}
			if directSetter {
				proxyCases, sizeCases = []bool{false}, []bool{false}
			}
			for _, proxy := range proxyCases {
				for _, large := range sizeCases {
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
						hookAttempted := make(chan struct{})
						hookReturned := make(chan struct{})
						installLifetimeRunner(t, func(req *runtime.ExecuteCodeRequest) error {
							req.Hooks.OnExecuteInit("session-1")
							select {
							case <-gate.started:
								if terminal != "returned-error" {
									close(hookAttempted)
									if terminal == "error" {
										req.Hooks.OnExecuteError(&execute.ErrorOutput{EName: "RuntimeError", EValue: message})
									} else {
										req.Hooks.OnExecuteComplete(time.Millisecond)
									}
									close(hookReturned)
								}
								return errors.New(message)
							case <-time.After(5 * time.Second):
								return errors.New("ping did not start")
							}
						})

						engine := gin.New()
						engine.POST(tc.route, func(ctx *gin.Context) {
							parent := &lifetimeParentContext{Context: ctx.Request.Context(), canceled: canceled}
							ctx.Request = ctx.Request.WithContext(parent)
							ctx.Writer = &lifetimeWriter{ResponseWriter: ctx.Writer, gate: gate, overlap: overlap}
							defer close(handlerDone)
							controller := NewCodeInterpretingController(ctx)
							if directSetter {
								// No outer WithCancel: AfterFunc identifies the context
								// created and stopped by setServerEventsHandler.
								hooks, stopSSE := controller.setServerEventsHandler(parent)
								defer stopSSE()
								_ = codeRunner.Execute(&runtime.ExecuteCodeRequest{Hooks: hooks})
								return
							}
							tc.run(controller)
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
							response, err := client.Post(endpoint+tc.path, "application/json", strings.NewReader(tc.body))
							if err != nil {
								resultCh <- result{err: err}
								return
							}
							defer response.Body.Close()
							body, err := io.ReadAll(response.Body)
							resultCh <- result{response: response, body: body, err: err}
						}()

						awaitSSELifetime(t, gate.started, "ping socket write")
						if terminal == "returned-error" {
							select {
							case <-canceled:
							case <-overlap:
								t.Fatal("error response was written while ping was still using the response writer")
							case <-time.After(5 * time.Second):
								t.Fatal("handler did not cancel ping after Execute returned an error")
							}
						} else {
							awaitSSELifetime(t, hookAttempted, "terminal hook attempt")
							if directSetter {
								select {
								case <-canceled:
								case <-hookReturned:
									t.Fatal("terminal hook returned without stopping its owned ping context")
								case <-overlap:
									t.Fatal("terminal hook wrote before stopping its owned ping context")
								case <-time.After(5 * time.Second):
									t.Fatal("terminal hook did not cancel its owned ping context before gate release")
								}
							}
							select {
							case <-hookReturned:
								t.Fatal("terminal hook returned before blocked ping stopped")
							default:
							}
						}
						select {
						case <-overlap:
							t.Fatal("error response overlapped the blocked ping")
						case <-handlerDone:
							t.Fatal("handler returned before blocked ping stopped")
						default:
						}
						release()
						if terminal != "returned-error" {
							awaitSSELifetime(t, hookReturned, "terminal hook return")
						}
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
						expectedTerminal := model.StreamEventTypeError
						if terminal == "complete" {
							expectedTerminal = model.StreamEventTypeComplete
						}
						require.Equal(t, expectedTerminal, events[len(events)-1].Type)
						var terminalsSeen int
						var errorsSeen int
						for _, event := range events {
							if event.Type == model.StreamEventTypeError || event.Type == model.StreamEventTypeComplete {
								terminalsSeen++
							}
							if event.Type == model.StreamEventTypeError {
								errorsSeen++
								require.NotNil(t, event.Error)
								require.Equal(t, "RuntimeError", event.Error.EName)
								require.Contains(t, event.Error.EValue, message)
							}
						}
						require.Equal(t, 1, terminalsSeen)
						if terminal == "complete" {
							require.Zero(t, errorsSeen)
						} else {
							require.Equal(t, 1, errorsSeen)
						}
					})
				}
			}
		})
	}
}

func TestExecutionReturnedErrorPreservesTerminalEvent(t *testing.T) {
	for _, tc := range lifetimeExecutionCases() {
		t.Run(tc.name, func(t *testing.T) {
			for _, terminalType := range []model.ServerStreamEventType{model.StreamEventTypeError, model.StreamEventTypeComplete} {
				t.Run(string(terminalType), func(t *testing.T) {
					installLifetimeRunner(t, func(req *runtime.ExecuteCodeRequest) error {
						req.Hooks.OnExecuteInit("session-1")
						if terminalType == model.StreamEventTypeError {
							req.Hooks.OnExecuteError(&execute.ErrorOutput{EName: "ContextCancelled", EValue: "Interrupt kernel"})
						} else {
							req.Hooks.OnExecuteComplete(time.Millisecond)
						}
						return errors.New("runtime returned error after terminal hook")
					})
					ctx, recorder := lifetimeTestContext(tc)
					tc.run(NewCodeInterpretingController(ctx))
					events := decodeLifetimeEvents(t, recorder.Body.Bytes())
					var terminals int
					for _, event := range events {
						if event.Type == model.StreamEventTypeError || event.Type == model.StreamEventTypeComplete {
							terminals++
							require.Equal(t, terminalType, event.Type)
							if event.Type == model.StreamEventTypeError {
								require.NotNil(t, event.Error)
								require.Equal(t, "ContextCancelled", event.Error.EName)
								require.Equal(t, "Interrupt kernel", event.Error.EValue)
							}
						}
					}
					require.Equal(t, 1, terminals, "Execute's returned error must not duplicate its terminal hook")
					require.Equal(t, terminalType, events[len(events)-1].Type)
					require.Equal(t, http.StatusOK, recorder.Code)
					require.Contains(t, recorder.Header().Get("Content-Type"), "text/event-stream")
				})
			}
		})
	}
}

func TestExecutionReturnedErrorBeforeInitPreservesJSON(t *testing.T) {
	for _, tc := range lifetimeExecutionCases() {
		t.Run(tc.name, func(t *testing.T) {
			installLifetimeRunner(t, func(*runtime.ExecuteCodeRequest) error {
				return errors.New("synchronous runtime failure")
			})
			ctx, recorder := lifetimeTestContext(tc)
			tc.run(NewCodeInterpretingController(ctx))
			require.Equal(t, http.StatusInternalServerError, recorder.Code)
			require.Contains(t, recorder.Header().Get("Content-Type"), "application/json")
			var response model.ErrorResponse
			require.NoError(t, json.Unmarshal(recorder.Body.Bytes(), &response))
			require.Equal(t, model.ErrorCodeRuntimeError, response.Code)
			require.Contains(t, response.Message, "synchronous runtime failure")
		})
	}
}
