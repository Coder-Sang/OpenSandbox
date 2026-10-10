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

package web

import (
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
	"github.com/stretchr/testify/require"
)

// A recorder cannot observe whether net/http terminates a chunked response
// gracefully after Gin recovers a ReverseProxy response-copy panic. Use real
// sockets and the production router so recovery and HTTP framing are exercised.
func TestNewRouterProxyResponseCopyFailureAbortsStream(t *testing.T) {
	for _, complete := range []bool{false, true} {
		name := "truncated"
		if complete {
			name = "complete"
		}
		t.Run(name, func(t *testing.T) {
			withTestBinding(t, nil)
			withRuntimeInit(t, false)

			const prefix = "data: first\n\n"
			release := make(chan struct{})
			releaseOnce := sync.OnceFunc(func() { close(release) })
			backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				conn, rw, err := w.(http.Hijacker).Hijack()
				if err != nil {
					t.Errorf("hijack backend: %v", err)
					return
				}
				defer conn.Close()
				_ = conn.SetWriteDeadline(time.Now().Add(5 * time.Second))
				_, err = fmt.Fprintf(rw, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n%x\r\n%s\r\n", len(prefix), prefix)
				if err == nil {
					err = rw.Flush()
				}
				if err != nil {
					t.Errorf("write backend prefix: %v", err)
					return
				}
				// Let the downstream receive committed data before inducing EOF.
				<-release
				if complete {
					if _, err := rw.WriteString("0\r\n\r\n"); err != nil {
						t.Errorf("write backend terminal chunk: %v", err)
						return
					}
					if err := rw.Flush(); err != nil {
						t.Errorf("flush backend terminal chunk: %v", err)
					}
				}
				// Without the terminal chunk the backend HTTP body is malformed.
			}))
			t.Cleanup(backend.Close)
			t.Cleanup(releaseOnce)

			backendURL, err := url.Parse(backend.URL)
			require.NoError(t, err)
			_, port, err := net.SplitHostPort(backendURL.Host)
			require.NoError(t, err)
			server := httptest.NewServer(NewRouter(""))
			t.Cleanup(func() {
				releaseOnce()
				server.Close()
			})
			client := server.Client()
			client.Timeout = 5 * time.Second
			resp, err := client.Get(server.URL + "/proxy/" + port + "/stream")
			require.NoError(t, err)
			defer resp.Body.Close()
			require.Equal(t, http.StatusOK, resp.StatusCode)
			require.Equal(t, "text/event-stream", resp.Header.Get("Content-Type"))
			first := make([]byte, len(prefix))
			_, err = io.ReadFull(resp.Body, first)
			require.NoError(t, err)
			require.Equal(t, prefix, string(first))
			releaseOnce()
			rest, err := io.ReadAll(resp.Body)
			require.Empty(t, rest, "no error response may be appended after commit")
			if complete {
				require.NoError(t, err)
			} else {
				require.ErrorIs(t, err, io.ErrUnexpectedEOF, "upstream failure must not become a successful downstream EOF")
			}
		})
	}
}

func TestNewRouterProxyWebSocketUpgradeStillWorks(t *testing.T) {
	withTestBinding(t, nil)
	withRuntimeInit(t, false)
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := (&websocket.Upgrader{}).Upgrade(w, r, nil)
		if err != nil {
			t.Errorf("upgrade backend: %v", err)
			return
		}
		defer conn.Close()
		_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
		typ, payload, err := conn.ReadMessage()
		if err != nil {
			t.Errorf("read backend message: %v", err)
			return
		}
		_ = conn.SetWriteDeadline(time.Now().Add(5 * time.Second))
		if err := conn.WriteMessage(typ, payload); err != nil {
			t.Errorf("echo backend message: %v", err)
		}
	}))
	defer backend.Close()
	backendURL, err := url.Parse(backend.URL)
	require.NoError(t, err)
	_, port, err := net.SplitHostPort(backendURL.Host)
	require.NoError(t, err)
	server := httptest.NewServer(NewRouter(""))
	defer server.Close()
	dialer := websocket.Dialer{HandshakeTimeout: 5 * time.Second}
	conn, resp, err := dialer.Dial("ws"+strings.TrimPrefix(server.URL, "http")+"/proxy/"+port+"/socket", nil)
	require.NoError(t, err)
	defer conn.Close()
	require.Equal(t, http.StatusSwitchingProtocols, resp.StatusCode)
	_ = conn.SetWriteDeadline(time.Now().Add(5 * time.Second))
	require.NoError(t, conn.WriteMessage(websocket.TextMessage, []byte("echo")))
	_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	typ, payload, err := conn.ReadMessage()
	require.NoError(t, err)
	require.Equal(t, websocket.TextMessage, typ)
	require.Equal(t, "echo", string(payload))
}

func TestNewRouterProxyFailureBeforeHeadersStillReturnsBadGateway(t *testing.T) {
	withTestBinding(t, nil)
	withRuntimeInit(t, false)
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, _, err := w.(http.Hijacker).Hijack()
		if err != nil {
			t.Errorf("hijack backend: %v", err)
			return
		}
		_ = conn.Close() // No HTTP response headers were sent.
	}))
	defer backend.Close()
	backendURL, err := url.Parse(backend.URL)
	require.NoError(t, err)
	_, port, err := net.SplitHostPort(backendURL.Host)
	require.NoError(t, err)
	server := httptest.NewServer(NewRouter(""))
	defer server.Close()
	client := server.Client()
	client.Timeout = 5 * time.Second
	resp, err := client.Get(server.URL + "/proxy/" + port + "/stream")
	require.NoError(t, err)
	defer resp.Body.Close()
	require.Equal(t, http.StatusBadGateway, resp.StatusCode)
	body, err := io.ReadAll(resp.Body)
	require.NoError(t, err)
	require.Equal(t, "Bad Gateway\n", string(body))
}

func TestNewRouterStillRecoversOrdinaryPanics(t *testing.T) {
	withTestBinding(t, nil)
	withRuntimeInit(t, false)
	router := NewRouter("")
	router.GET("/test-panic", func(c *gin.Context) { panic("ordinary handler failure") })
	server := httptest.NewServer(router)
	defer server.Close()
	client := server.Client()
	client.Timeout = 5 * time.Second
	resp, err := client.Get(server.URL + "/test-panic")
	require.NoError(t, err)
	defer resp.Body.Close()
	require.Equal(t, http.StatusInternalServerError, resp.StatusCode)
	_, err = io.ReadAll(resp.Body)
	require.NoError(t, err)
}
