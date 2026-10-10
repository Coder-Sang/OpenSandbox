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

//go:build !windows

package controller

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/http/httputil"
	"net/url"
	"path/filepath"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"

	"github.com/alibaba/opensandbox/execd/pkg/flag"
	realruntime "github.com/alibaba/opensandbox/execd/pkg/runtime"
	"github.com/alibaba/opensandbox/execd/pkg/web/model"
)

// These tests deliberately use a real runtime, but neither start Jupyter nor
// launch a child process. Both errors occur after the runtime emits init and
// are returned synchronously without an OnExecuteError callback.
func TestRealRuntimeReturnedErrorHTTP(t *testing.T) {
	for _, operation := range []string{"RunCommand/background", "RunInSession"} {
		t.Run(operation, func(t *testing.T) {
			for _, proxied := range []bool{false, true} {
				name := "direct"
				if proxied {
					name = "reverse-proxy"
				}
				t.Run(name, func(t *testing.T) {
					// Global runner, flags, and TMPDIR require sequential subtests.
					workspace := t.TempDir()
					t.Setenv("TMPDIR", workspace)
					runner := realruntime.NewController("", "")
					previousRunner := codeRunner
					previousGrace := flag.ApiGracefulShutdownTimeout
					codeRunner = runner
					flag.ApiGracefulShutdownTimeout = 0
					t.Cleanup(func() {
						codeRunner = previousRunner
						flag.ApiGracefulShutdownTimeout = previousGrace
					})

					var route, path, expectedError, sessionID string
					var run func(*CodeInterpretingController)
					var request any
					if operation == "RunCommand/background" {
						route, path = "/command", "/command"
						run = (*CodeInterpretingController).RunCommand
						missingExecutable := filepath.Join(workspace, "nonexistent-executable")
						request = map[string]any{"argv": []string{missingExecutable}, "background": true}
						expectedError = missingExecutable
					} else {
						var err error
						sessionID, err = runner.CreateBashSession(&realruntime.CreateContextRequest{Cwd: workspace})
						require.NoError(t, err)
						t.Cleanup(func() { require.NoError(t, runner.DeleteBashSession(sessionID)) })
						// CreateTemp must fail before any shell is launched or script
						// is written. The absent directory stays inside our workspace.
						missingTempDir := filepath.Join(workspace, "nonexistent-temp-directory")
						t.Setenv("TMPDIR", missingTempDir)
						route, path = "/session/:sessionId/run", "/session/"+sessionID+"/run"
						run = (*CodeInterpretingController).RunInSession
						request = map[string]any{"command": "true", "timeout": 0}
						expectedError = missingTempDir
					}
					body, err := json.Marshal(request)
					require.NoError(t, err)
					engine := gin.New()
					engine.POST(route, func(ctx *gin.Context) { run(NewCodeInterpretingController(ctx)) })
					server := httptest.NewServer(engine)
					defer server.Close()
					endpoint := server.URL
					if proxied {
						upstream, err := url.Parse(endpoint)
						require.NoError(t, err)
						gateway := httptest.NewServer(httputil.NewSingleHostReverseProxy(upstream))
						defer gateway.Close()
						endpoint = gateway.URL
					}
					transport := &http.Transport{DisableKeepAlives: true}
					defer transport.CloseIdleConnections()
					client := &http.Client{Transport: transport, Timeout: 5 * time.Second}
					response, err := client.Post(endpoint+path, "application/json", bytes.NewReader(body))
					require.NoError(t, err)
					defer response.Body.Close()
					wireBody, err := io.ReadAll(response.Body)
					require.NoError(t, err, "real chunked HTTP response must finish with clean EOF")
					require.Equal(t, http.StatusOK, response.StatusCode)
					require.Equal(t, []string{"chunked"}, response.TransferEncoding)
					require.Contains(t, response.Header.Get("Content-Type"), "text/event-stream")

					var events []model.ServerStreamEvent
					for _, frame := range bytes.Split(bytes.TrimSpace(wireBody), []byte("\n\n")) {
						var event model.ServerStreamEvent
						require.NoError(t, json.Unmarshal(frame, &event), "invalid SSE frame: %q", frame)
						require.NotEmpty(t, event.Type, "ordinary JSON errors must not be appended after SSE init: %q", frame)
						events = append(events, event)
					}
					require.NotEmpty(t, events)
					require.Equal(t, model.StreamEventTypeInit, events[0].Type)
					require.Equal(t, model.StreamEventTypeError, events[len(events)-1].Type)
					var errorsSeen int
					for _, event := range events {
						if event.Type == model.StreamEventTypeError {
							errorsSeen++
							require.NotNil(t, event.Error)
							require.Equal(t, "RuntimeError", event.Error.EName)
							require.Contains(t, event.Error.EValue, expectedError)
						}
					}
					require.Equal(t, 1, errorsSeen, "returned runtime error must yield exactly one terminal event")
				})
			}
		})
	}
}
