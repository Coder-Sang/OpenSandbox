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
	"testing"
	"time"

	"github.com/stretchr/testify/require"

	"github.com/alibaba/opensandbox/execd/pkg/jupyter/execute"
)

func TestDispatchExecutionResultHooks_OutputBeforeTerminal(t *testing.T) {
	for _, failed := range []bool{false, true} {
		t.Run(map[bool]string{false: "success", true: "error"}[failed], func(t *testing.T) {
			var events []string
			req := &ExecuteCodeRequest{Hooks: ExecuteResultHook{
				OnExecuteResult: func(data map[string]any, count int) {
					require.Equal(t, 7, count)
					require.Equal(t, "value", data["text/plain"])
					events = append(events, "result")
				},
				OnExecuteStatus:   func(string) { events = append(events, "status") },
				OnExecuteStdout:   func(text string) { events = append(events, "stdout:"+text) },
				OnExecuteStderr:   func(text string) { events = append(events, "stderr:"+text) },
				OnExecuteError:    func(*execute.ErrorOutput) { events = append(events, "error") },
				OnExecuteComplete: func(time.Duration) { events = append(events, "complete") },
			}}
			result := &execute.ExecutionResult{
				ExecutionCount: 7, ExecutionData: map[string]any{"text/plain": "value"},
				Status: "idle", ExecutionTime: time.Millisecond,
				Stream: []*execute.StreamOutput{{Name: execute.StreamStdout, Text: "before"}, {Name: execute.StreamStderr, Text: "warning"}},
			}
			terminal := "complete"
			if failed {
				result.Error = &execute.ErrorOutput{EName: "RuntimeError"}
				terminal = "error"
			}
			dispatchExecutionResultHooks(req, result)
			require.Equal(t, []string{"result", "status", "stdout:before", "stderr:warning", terminal}, events)
		})
	}
}

func TestDispatchExecutionResultHooks_ErrorSkipsComplete(t *testing.T) {
	var (
		errorCalls    int
		completeCalls int
	)

	req := &ExecuteCodeRequest{
		Hooks: ExecuteResultHook{
			OnExecuteError: func(_ *execute.ErrorOutput) {
				errorCalls++
			},
			OnExecuteComplete: func(_ time.Duration) {
				completeCalls++
			},
		},
	}

	dispatchExecutionResultHooks(req, &execute.ExecutionResult{
		ExecutionTime: 35 * time.Millisecond,
		Error: &execute.ErrorOutput{
			EName:  "RuntimeError",
			EValue: "boom",
		},
	})

	require.Equal(t, 1, errorCalls)
	require.Equal(t, 0, completeCalls)
}

func TestDispatchExecutionResultHooks_SuccessEmitsComplete(t *testing.T) {
	var (
		errorCalls    int
		completeCalls int
	)

	req := &ExecuteCodeRequest{
		Hooks: ExecuteResultHook{
			OnExecuteError: func(_ *execute.ErrorOutput) {
				errorCalls++
			},
			OnExecuteComplete: func(_ time.Duration) {
				completeCalls++
			},
		},
	}

	dispatchExecutionResultHooks(req, &execute.ExecutionResult{
		ExecutionTime: 50 * time.Millisecond,
	})

	require.Equal(t, 0, errorCalls)
	require.Equal(t, 1, completeCalls)
}
