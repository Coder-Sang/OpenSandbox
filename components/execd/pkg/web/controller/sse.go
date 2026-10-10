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

package controller

import (
	"context"
	"io"
	"net/http"
	"sync"
	"time"

	"github.com/alibaba/opensandbox/internal/safego"
	"k8s.io/apimachinery/pkg/util/wait"

	"github.com/alibaba/opensandbox/execd/pkg/jupyter/execute"
	"github.com/alibaba/opensandbox/execd/pkg/log"
	"github.com/alibaba/opensandbox/execd/pkg/runtime"
	"github.com/alibaba/opensandbox/execd/pkg/web/model"
)

var sseHeaders = map[string]string{
	"Content-Type":      "text/event-stream",
	"Cache-Control":     "no-cache",
	"Connection":        "keep-alive",
	"X-Accel-Buffering": "no",
}

// respondExecutionError handles an error returned by the runtime. Callers must
// cancel and join the ping writer first, before inspecting the response or
// writing JSON, since Gin and net/http response writers are not concurrency-safe.
func (c *CodeInterpretingController) respondExecutionError(completeCh <-chan struct{}, hooks runtime.ExecuteResultHook, message string) {
	select {
	case <-completeCh:
		// The runtime already delivered a terminal event.
		return
	default:
	}
	if c.ctx.Writer.Written() {
		// Once SSE is committed, a JSON error would corrupt the stream.
		hooks.OnExecuteError(&execute.ErrorOutput{EName: "RuntimeError", EValue: message})
	} else {
		c.RespondError(http.StatusInternalServerError, model.ErrorCodeRuntimeError, message)
	}
}

// setupSSEResponse is idempotent: once headers are committed, subsequent calls
// no-op. Callers that need the headers up front (e.g. long-running streaming
// endpoints with no early-error path) can call it explicitly. Endpoints that
// may fail synchronously before any event fires should leave header commit to
// the lazy path inside writeSingleEvent so pre-execution errors can return a
// proper JSON body instead of a half-formed text/event-stream response.
func (c *basicController) setupSSEResponse() {
	c.sseSetupOnce.Do(func() {
		for key, value := range sseHeaders {
			c.ctx.Writer.Header().Set(key, value)
		}
		if flusher, ok := c.ctx.Writer.(http.Flusher); ok {
			flusher.Flush()
		}
	})
}

// setServerEventsHandler adapts runtime callbacks to SSE events.
//
// It returns the hooks and a cleanup function. The cleanup function cancels
// the heartbeat and blocks until its goroutine has fully stopped. Terminal
// hooks invoke it before writing their final event. Callers MUST also invoke
// it before their handler returns so that no goroutine
// races against the response-writer close that Go's net/http performs once
// the handler exits.
//
// Typical usage:
//
//	hooks, stopSSE := c.setServerEventsHandler(ctx)
//	defer func() { cancel(); stopSSE() }()
func (c *CodeInterpretingController) setServerEventsHandler(ctx context.Context) (runtime.ExecuteResultHook, func()) {
	var pingWg sync.WaitGroup
	pingCtx, cancelPing := context.WithCancel(ctx)
	stopSSE := func() {
		cancelPing()
		pingWg.Wait()
	}

	hooks := runtime.ExecuteResultHook{
		OnExecuteInit: func(session string) {
			event := model.ServerStreamEvent{
				Type:      model.StreamEventTypeInit,
				Text:      session,
				Timestamp: time.Now().UnixMilli(),
			}
			payload := event.ToJSON()
			c.writeSingleEvent("OnExecuteInit", payload, true, event.Summary())

			pingWg.Add(1)
			safego.Go(func() {
				defer pingWg.Done()
				c.ping(pingCtx)
			})
		},
		OnExecuteResult: func(result map[string]any, count int) {
			var mutated map[string]any
			if len(result) > 0 {
				mutated = make(map[string]any)
				for k, v := range result {
					switch k {
					case "text/plain":
						mutated["text"] = v
					default:
						mutated[k] = v
					}
				}
			}

			if count > 0 {
				event := model.ServerStreamEvent{
					Type:           model.StreamEventTypeCount,
					ExecutionCount: count,
					Timestamp:      time.Now().UnixMilli(),
				}
				payload := event.ToJSON()
				c.writeSingleEvent("OnExecuteResult", payload, true, event.Summary())
			}
			if len(mutated) > 0 {
				event := model.ServerStreamEvent{
					Type:      model.StreamEventTypeResult,
					Results:   mutated,
					Timestamp: time.Now().UnixMilli(),
				}
				payload := event.ToJSON()
				c.writeSingleEvent("OnExecuteResult", payload, true, event.Summary())
			}
		},
		OnExecuteComplete: func(executionTime time.Duration) {
			// A queued heartbeat must not follow the terminal event.
			stopSSE()
			event := model.ServerStreamEvent{
				Type:          model.StreamEventTypeComplete,
				ExecutionTime: executionTime.Milliseconds(),
				Timestamp:     time.Now().UnixMilli(),
			}
			payload := event.ToJSON()
			c.writeSingleEvent("OnExecuteComplete", payload, true, event.Summary())
		},
		OnExecuteError: func(err *execute.ErrorOutput) {
			if err == nil {
				return
			}
			stopSSE()

			event := model.ServerStreamEvent{
				Type:      model.StreamEventTypeError,
				Error:     err,
				Timestamp: time.Now().UnixMilli(),
			}
			payload := event.ToJSON()
			c.writeSingleEvent("OnExecuteError", payload, true, event.Summary())
		},
		OnExecuteStatus: func(status string) {
			event := model.ServerStreamEvent{
				Type:      model.StreamEventTypeStatus,
				Text:      status,
				Timestamp: time.Now().UnixMilli(),
			}
			payload := event.ToJSON()
			c.writeSingleEvent("OnExecuteStatus", payload, true, event.Summary())
		},
		OnExecuteStdout: func(text string) {
			if text == "" {
				return
			}

			event := model.ServerStreamEvent{
				Type:      model.StreamEventTypeStdout,
				Text:      text,
				Timestamp: time.Now().UnixMilli(),
			}
			payload := event.ToJSON()
			c.writeSingleEvent("OnExecuteStdout", payload, true, event.Summary())
		},
		OnExecuteStderr: func(text string) {
			if text == "" {
				return
			}

			event := model.ServerStreamEvent{
				Type:      model.StreamEventTypeStderr,
				Text:      text,
				Timestamp: time.Now().UnixMilli(),
			}
			payload := event.ToJSON()
			c.writeSingleEvent("OnExecuteStderr", payload, true, event.Summary())
		},
	}

	return hooks, stopSSE
}

func (c *basicController) writeSingleEvent(handler string, data []byte, verbose bool, summary string) {
	if c == nil || c.ctx == nil || c.ctx.Writer == nil {
		return
	}

	select {
	case <-c.ctx.Request.Context().Done():
		log.Error("sse: %s client disconnected", handler)
		return
	default:
	}

	c.chunkWriter.Lock()
	defer c.chunkWriter.Unlock()
	// Lazily commit SSE response headers on the first event. This lets the
	// surrounding handler return a proper JSON error via RespondError if the
	// runtime fails synchronously before any event fires.
	c.setupSSEResponse()
	defer func() {
		if flusher, ok := c.ctx.Writer.(http.Flusher); ok {
			flusher.Flush()
		}
	}()

	payload := append(data, '\n', '\n')
	n, err := c.ctx.Writer.Write(payload)
	if err == nil && n != len(payload) {
		err = io.ErrShortWrite
	}

	if err != nil {
		log.Error("sse: %s write %s: %v", handler, summary, err)
	} else {
		if verbose {
			log.Info("sse: %s write %s", handler, summary)
		}
	}
}

// ping periodically keeps the SSE connection alive.
func (c *CodeInterpretingController) ping(ctx context.Context) {
	wait.Until(func() {
		if c.ctx.Writer == nil {
			return
		}
		event := model.ServerStreamEvent{
			Type:      model.StreamEventTypePing,
			Text:      "pong",
			Timestamp: time.Now().UnixMilli(),
		}
		payload := event.ToJSON()
		c.writeSingleEvent("Ping", payload, false, event.Summary())
	}, 3*time.Second, ctx.Done())
}
