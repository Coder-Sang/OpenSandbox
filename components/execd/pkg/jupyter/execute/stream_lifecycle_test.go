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

package execute

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/stretchr/testify/require"
)

func sendKernelMessage(t *testing.T, conn *websocket.Conn, parent Header, kind MessageType, content interface{}) {
	t.Helper()
	data, err := json.Marshal(content)
	require.NoError(t, err)
	require.NoError(t, conn.WriteJSON(Message{Header: Header{MessageType: string(kind)}, ParentHeader: parent, Content: data}))
}

func collectExecution(t *testing.T, results <-chan *ExecutionResult) []*ExecutionResult {
	t.Helper()
	var got []*ExecutionResult
	timer := time.NewTimer(2 * time.Second)
	defer timer.Stop()
	for {
		select {
		case result, ok := <-results:
			if !ok {
				return got
			}
			got = append(got, result)
		case <-timer.C:
			t.Fatal("execution channel did not close")
			return nil
		}
	}
}

func TestRequestBoundCompletionOrdering(t *testing.T) {
	for _, idleFirst := range []bool{false, true} {
		for _, output := range []string{"none", "display", "error", "reply-error"} {
			name := output + "/reply-first"
			if idleFirst {
				name = output + "/idle-first"
			}
			t.Run(name, func(t *testing.T) {
				release := make(chan struct{})
				server := createTestServer(t, func(conn *websocket.Conn) {
					var request Message
					require.NoError(t, conn.ReadJSON(&request))
					foreign := Header{MessageID: "foreign"}
					sendKernelMessage(t, conn, foreign, MsgStream, StreamOutput{Text: "foreign"})
					sendKernelMessage(t, conn, foreign, MsgDisplayData, DisplayData{Data: map[string]interface{}{"text/plain": "foreign"}})
					sendKernelMessage(t, conn, foreign, MsgExecuteResult, ExecuteResult{ExecutionCount: 9})
					sendKernelMessage(t, conn, foreign, MsgError, ErrorOutput{EName: "foreign"})
					sendKernelMessage(t, conn, foreign, MsgExecuteReply, ExecuteReply{Status: "ok", ExecutionCount: 9})
					sendKernelMessage(t, conn, foreign, MsgStatus, StatusUpdate{ExecutionState: StateIdle})
					if idleFirst {
						sendKernelMessage(t, conn, request.Header, MsgStatus, StatusUpdate{ExecutionState: StateIdle})
					}
					if output == "display" {
						sendKernelMessage(t, conn, request.Header, MsgDisplayData, DisplayData{Data: map[string]interface{}{"image/png": "encoded", "text/html": "<b>hello</b>"}})
					}
					reply := ExecuteReply{Status: "ok", ExecutionCount: 0}
					if output == "error" || output == "reply-error" {
						reply.Status = "error"
						reply.ErrorOutput = ErrorOutput{EName: "ValueError", EValue: "bad"}
						if output == "error" {
							sendKernelMessage(t, conn, request.Header, MsgError, reply.ErrorOutput)
						}
					}
					sendKernelMessage(t, conn, request.Header, MsgExecuteReply, reply)
					if !idleFirst {
						sendKernelMessage(t, conn, request.Header, MsgStatus, StatusUpdate{ExecutionState: StateIdle})
					}
					<-release
				})
				defer server.Close()
				defer close(release)
				client := NewClient("", nil)
				require.NoError(t, client.Connect("ws"+strings.TrimPrefix(server.URL, "http")+"/api/kernels/test/channels"))
				defer client.Disconnect()
				results := make(chan *ExecutionResult, 10)
				require.NoError(t, client.ExecuteCodeStream("code", results))
				got := collectExecution(t, results)
				expected := 1
				if output != "none" {
					expected = 2
				}
				require.Len(t, got, expected)
				require.Positive(t, got[len(got)-1].ExecutionTime)
				if output == "display" {
					require.Equal(t, "encoded", got[0].ExecutionData["image/png"])
				}
				if output == "error" || output == "reply-error" {
					require.Equal(t, "ValueError", got[0].Error.EName)
				}
			})
		}
	}
}

func TestCallbackWriteResetsPreviousStreamDeadline(t *testing.T) {
	requestReceived := make(chan struct{})
	server := createTestServer(t, func(conn *websocket.Conn) {
		var request Message
		if conn.ReadJSON(&request) != nil {
			return
		}
		close(requestReceived)
	})
	defer server.Close()
	client := NewClient("", nil)
	require.NoError(t, client.Connect("ws"+strings.TrimPrefix(server.URL, "http")+"/api/kernels/test/channels"))
	defer client.Disconnect()
	client.writeMu.Lock()
	require.NoError(t, client.conn.SetWriteDeadline(time.Now().Add(-time.Second)))
	client.writeMu.Unlock()
	require.NoError(t, client.ExecuteCodeWithCallback("code", CallbackHandler{}))
	select {
	case <-requestReceived:
	case <-time.After(2 * time.Second):
		t.Fatal("legacy callback request was not delivered")
	}
}

func TestCancellationUnblocksPendingWebsocketWrite(t *testing.T) {
	connected := make(chan struct{})
	release := make(chan struct{})
	server := createTestServer(t, func(conn *websocket.Conn) {
		close(connected)
		<-release // The peer deliberately never reads the execution body.
	})
	defer server.Close()
	defer close(release)
	client := NewClient("", nil)
	require.NoError(t, client.Connect("ws"+strings.TrimPrefix(server.URL, "http")+"/api/kernels/test/channels"))
	defer client.Disconnect()
	<-connected
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	results := make(chan *ExecutionResult, 1)
	returned := make(chan error, 1)
	go func() { returned <- client.ExecuteCodeStreamContext(ctx, strings.Repeat("x", 16<<20), results) }()
	// Serialize with handler installation, which precedes the potentially blocked write.
	installed := make(chan struct{})
	go func() {
		for {
			client.mu.Lock()
			ready := client.handlers[MsgExecuteReply] != nil
			client.mu.Unlock()
			if ready {
				close(installed)
				return
			}
			select {
			case <-ctx.Done():
				return
			default:
			}
		}
	}()
	select {
	case <-installed:
	case <-time.After(2 * time.Second):
		t.Fatal("request not installed")
	}
	cancel()
	select {
	case err := <-returned:
		require.Error(t, err)
	case <-time.After(2 * time.Second):
		t.Fatal("cancellation did not unblock websocket write")
	}
	collectExecution(t, results)
}

func TestReconnectDoesNotReuseOldReader(t *testing.T) {
	requests := make(chan struct{}, 2)
	release := make(chan struct{})
	server := createTestServer(t, func(conn *websocket.Conn) {
		var request Message
		if conn.ReadJSON(&request) != nil {
			return
		}
		requests <- struct{}{}
		data, _ := json.Marshal(ExecuteReply{Status: "ok"})
		if conn.WriteJSON(Message{Header: Header{MessageType: string(MsgExecuteReply)}, ParentHeader: request.Header, Content: data}) != nil {
			return
		}
		data, _ = json.Marshal(StatusUpdate{ExecutionState: StateIdle})
		if conn.WriteJSON(Message{Header: Header{MessageType: string(MsgStatus)}, ParentHeader: request.Header, Content: data}) != nil {
			return
		}
		<-release
	})
	defer server.Close()
	defer close(release)
	client := NewClient("", nil)
	defer client.Disconnect()
	url := "ws" + strings.TrimPrefix(server.URL, "http") + "/api/kernels/test/channels"
	for i := 0; i < 2; i++ {
		require.NoError(t, client.Connect(url))
		results := make(chan *ExecutionResult, 1)
		require.NoError(t, client.ExecuteCodeStream("code", results))
		<-requests
		got := collectExecution(t, results)
		require.Len(t, got, 1)
		require.Positive(t, got[0].ExecutionTime)
		client.Disconnect()
	}
}

func TestFullChannelCancellationAndDisconnect(t *testing.T) {
	for _, disconnect := range []bool{false, true} {
		name := "cancel"
		if disconnect {
			name = "disconnect"
		}
		t.Run(name, func(t *testing.T) {
			sent := make(chan struct{})
			release := make(chan struct{})
			server := createTestServer(t, func(conn *websocket.Conn) {
				var request Message
				require.NoError(t, conn.ReadJSON(&request))
				for i := 0; i < 3; i++ {
					sendKernelMessage(t, conn, request.Header, MsgStream, StreamOutput{Text: "output"})
				}
				close(sent)
				<-release
			})
			defer server.Close()
			defer close(release)
			client := NewClient("", nil)
			require.NoError(t, client.Connect("ws"+strings.TrimPrefix(server.URL, "http")+"/api/kernels/test/channels"))
			defer client.Disconnect()
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			results := make(chan *ExecutionResult, 1)
			require.NoError(t, client.ExecuteCodeStreamContext(ctx, "code", results))
			<-sent
			if disconnect {
				client.Disconnect()
			} else {
				cancel()
			}
			got := collectExecution(t, results)
			for _, result := range got {
				require.Zero(t, result.ExecutionTime)
			}
			client.Disconnect()
		})
	}
}
