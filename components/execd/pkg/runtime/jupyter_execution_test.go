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

package runtime

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/alibaba/opensandbox/execd/pkg/jupyter"
	"github.com/alibaba/opensandbox/execd/pkg/jupyter/execute"
	"github.com/gorilla/websocket"
	"github.com/stretchr/testify/require"
)

func TestRunJupyterCodeImmediateTerminalReuse(t *testing.T) {
	for _, failed := range []bool{false, true} {
		t.Run(map[bool]string{false: "complete", true: "error"}[failed], func(t *testing.T) {
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			var connections atomic.Int32
			closed := make(chan struct{}, 2)
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				conn, err := (&websocket.Upgrader{}).Upgrade(w, r, nil)
				if err != nil {
					t.Errorf("upgrade: %v", err)
					return
				}
				defer conn.Close()
				defer func() { closed <- struct{}{} }()
				index := connections.Add(1)
				var request execute.Message
				if err := conn.ReadJSON(&request); err != nil {
					t.Errorf("read: %v", err)
					return
				}
				send := func(kind execute.MessageType, value any) {
					body, _ := json.Marshal(value)
					if err := conn.WriteJSON(execute.Message{Header: execute.Header{MessageType: string(kind)}, ParentHeader: request.Header, Content: body}); err != nil {
						t.Errorf("write: %v", err)
					}
				}
				reply := execute.ExecuteReply{Status: "ok", ExecutionCount: int(index)}
				if failed && index == 1 {
					reply.Status = "error"
					reply.ErrorOutput = execute.ErrorOutput{EName: "RuntimeError"}
					send(execute.MsgError, reply.ErrorOutput)
				}
				send(execute.MsgExecuteReply, reply)
				send(execute.MsgStatus, execute.StatusUpdate{ExecutionState: execute.StateIdle})
				for {
					if _, _, err := conn.ReadMessage(); err != nil {
						return
					}
				}
			}))
			defer server.Close()
			kernel := &jupyterKernel{client: jupyter.NewClient(server.URL), kernelID: "test"}
			controller := &Controller{}
			var counts []int
			terminalCalls, secondComplete := 0, 0
			reuse := func() {
				terminalCalls++
				locked := kernel.mu.TryLock()
				require.True(t, locked, "terminal exposed before releasing session")
				if locked {
					kernel.mu.Unlock()
				}
				second := &ExecuteCodeRequest{Code: "second", Hooks: ExecuteResultHook{
					OnExecuteComplete: func(time.Duration) { secondComplete++ },
					OnExecuteError:    func(e *execute.ErrorOutput) { t.Errorf("second execution failed: %v", e) },
				}}
				second.SetDefaultHooks()
				require.NoError(t, controller.runJupyterCode(ctx, kernel, second))
			}
			first := &ExecuteCodeRequest{Code: "first", Hooks: ExecuteResultHook{
				OnExecuteResult: func(_ map[string]any, count int) {
					counts = append(counts, count)
					locked := kernel.mu.TryLock()
					require.False(t, locked, "output must precede session release")
					if locked {
						kernel.mu.Unlock()
					}
				},
				OnExecuteError:    func(*execute.ErrorOutput) { require.True(t, failed); reuse() },
				OnExecuteComplete: func(time.Duration) { require.False(t, failed); reuse() },
			}}
			first.SetDefaultHooks()
			require.NoError(t, controller.runJupyterCode(ctx, kernel, first))
			require.Equal(t, []int{1}, counts)
			require.Equal(t, 1, terminalCalls)
			require.Equal(t, 1, secondComplete)
			require.EqualValues(t, 2, connections.Load())
			for i := 0; i < 2; i++ {
				select {
				case <-closed:
				case <-ctx.Done():
					t.Fatal("execution connection not closed")
				}
			}
		})
	}
}

func TestRunJupyterCodeCancellation(t *testing.T) {
	for _, terminal := range []string{"cancel", "complete", "error", "disconnect"} {
		t.Run(terminal, func(t *testing.T) {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			var interrupts atomic.Int32
			disconnected := make(chan struct{})
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				if strings.HasSuffix(r.URL.Path, "/interrupt") {
					interrupts.Add(1)
					w.WriteHeader(http.StatusNoContent)
					return
				}
				conn, err := (&websocket.Upgrader{}).Upgrade(w, r, nil)
				if err != nil {
					t.Errorf("upgrade: %v", err)
					return
				}
				defer conn.Close()
				defer close(disconnected)
				var request execute.Message
				if err := conn.ReadJSON(&request); err != nil {
					t.Errorf("read: %v", err)
					return
				}
				if terminal == "disconnect" {
					return
				}
				if terminal == "cancel" {
					cancel()
				} else {
					send := func(kind execute.MessageType, content any) {
						body, err := json.Marshal(content)
						if err != nil {
							t.Errorf("marshal: %v", err)
							return
						}
						if err := conn.WriteJSON(execute.Message{Header: execute.Header{MessageType: string(kind)}, ParentHeader: request.Header, Content: body}); err != nil {
							t.Errorf("write: %v", err)
						}
					}
					send(execute.MsgStream, execute.StreamOutput{Name: execute.StreamStdout, Text: "before"})
					reply := execute.ExecuteReply{Status: "ok", ExecutionCount: 1}
					if terminal == "error" {
						reply.Status = "error"
						reply.ErrorOutput = execute.ErrorOutput{EName: "RuntimeError", EValue: "boom"}
						send(execute.MsgError, reply.ErrorOutput)
					}
					if terminal == "error" {
						send(execute.MsgStream, execute.StreamOutput{Name: execute.StreamStderr, Text: "after-error"})
					}
					send(execute.MsgExecuteReply, reply)
					send(execute.MsgStatus, execute.StatusUpdate{ExecutionState: execute.StateIdle})
				}
				for {
					if _, _, err := conn.ReadMessage(); err != nil {
						return
					}
				}
			}))
			defer server.Close()
			var events []string
			req := &ExecuteCodeRequest{Code: "print('before')", Hooks: ExecuteResultHook{
				OnExecuteStdout: func(s string) { events = append(events, s) },
				OnExecuteStderr: func(s string) { events = append(events, s) },
				OnExecuteResult: func(_ map[string]any, count int) {
					if count > 0 {
						events = append(events, "count")
					}
				},
				OnExecuteError:    func(e *execute.ErrorOutput) { events = append(events, e.EName); cancel() },
				OnExecuteComplete: func(time.Duration) { events = append(events, "complete"); cancel() },
			}}
			req.SetDefaultHooks()
			kernel := &jupyterKernel{client: jupyter.NewClient(server.URL), kernelID: "test"}
			done := make(chan error, 1)
			go func() { done <- (&Controller{}).runJupyterCode(ctx, kernel, req) }()
			select {
			case err := <-done:
				if terminal == "disconnect" {
					require.Error(t, err)
					require.Equal(t, []string{"ExecutionInterrupted"}, events)
					require.Zero(t, interrupts.Load())
				} else if terminal == "cancel" {
					require.Error(t, err)
					require.Equal(t, []string{"ContextCancelled"}, events)
					require.EqualValues(t, 1, interrupts.Load())
				} else {
					require.NoError(t, err)
					want := "complete"
					if terminal == "error" {
						want = "RuntimeError"
					}
					expected := []string{"before", "count", want}
					if terminal == "error" {
						expected = []string{"before", "after-error", "count", want}
					}
					require.Equal(t, expected, events)
					require.Zero(t, interrupts.Load())
				}
			case <-time.After(5 * time.Second):
				t.Fatal("runtime did not stop")
			}
			select {
			case <-disconnected:
			case <-time.After(time.Second):
				t.Fatal("producer connection not closed")
			}
			require.True(t, kernel.mu.TryLock())
			kernel.mu.Unlock()
		})
	}
}
