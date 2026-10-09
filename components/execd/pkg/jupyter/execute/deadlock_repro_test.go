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
	"runtime"
	"testing"
	"time"
)

// A blocked output send must neither hold resultMutex nor survive cancellation.
func TestResultMutexNotHeldAcrossBlockingChannelSend(t *testing.T) {
	c := &Client{}
	state := newStreamExecutionState(time.Now())
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	state.ctx = ctx
	results := make(chan *ExecutionResult, 1)
	content, _ := json.Marshal(StreamOutput{Name: StreamStdout, Text: "line"})
	msg := &Message{Content: content}
	c.handleStreamOutput(msg, state, results)
	returned := make(chan struct{})
	go func() { c.handleStreamOutput(msg, state, results); close(returned) }()
	// Only the second sender can hold chanMutex now. Observing its read lock
	// proves it reached trySend on the full channel before checking resultMutex.
	deadline := time.Now().Add(time.Second)
	for state.chanMutex.TryLock() {
		state.chanMutex.Unlock()
		if time.Now().After(deadline) {
			t.Fatal("second sender did not reach the full-channel send")
		}
		runtime.Gosched()
	}
	acquired := make(chan struct{})
	go func() { state.resultMutex.Lock(); state.resultMutex.Unlock(); close(acquired) }()
	select {
	case <-acquired:
	case <-time.After(time.Second):
		t.Fatal("output send held resultMutex")
	}
	cancel()
	select {
	case <-returned:
	case <-time.After(time.Second):
		t.Fatal("output send survived cancellation")
	}
	state.closeResultChan(results)
	state.closeResultChan(results)
}
