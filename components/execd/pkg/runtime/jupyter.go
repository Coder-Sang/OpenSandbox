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

package runtime

import (
	"context"
	"errors"
	"sort"
	"sync"

	"github.com/alibaba/opensandbox/execd/pkg/jupyter"
	"github.com/alibaba/opensandbox/execd/pkg/jupyter/execute"
	"github.com/alibaba/opensandbox/execd/pkg/log"
)

// runJupyter executes code through a Jupyter kernel.
func (c *Controller) runJupyter(ctx context.Context, request *ExecuteCodeRequest) error {
	if c.baseURL == "" || c.token == "" {
		return errors.New("language runtime server not configured, please check your image runtime")
	}
	if request.Context == "" {
		if c.getDefaultLanguageSession(request.Language) == "" {
			if err := c.createDefaultLanguageJupyterContext(request.Language); err != nil {
				return err
			}
		}
	}

	var targetSessionID string
	if request.Context == "" {
		targetSessionID = c.getDefaultLanguageSession(request.Language)
	} else {
		targetSessionID = request.Context
	}

	kernel := c.getJupyterKernel(targetSessionID)
	if kernel == nil {
		return ErrContextNotFound
	}

	request.SetDefaultHooks()
	request.Hooks.OnExecuteInit(targetSessionID)

	return c.runJupyterCode(ctx, kernel, request)
}

// runJupyterCode streams execution results for a single kernel.
//
//nolint:gocognit // complex due to hook handling; refactor later
func (c *Controller) runJupyterCode(ctx context.Context, kernel *jupyterKernel, request *ExecuteCodeRequest) error {
	if !kernel.mu.TryLock() {
		return errors.New("session is busy")
	}
	streamCtx, cancelStream := context.WithCancel(ctx)
	var cleanupOnce sync.Once
	cleanup := func() {
		cleanupOnce.Do(func() {
			cancelStream()
			kernel.client.DisconnectFromKernel()
			kernel.mu.Unlock()
		})
	}
	defer cleanup()

	err := kernel.client.ConnectToKernel(kernel.kernelID)
	if err != nil {
		return err
	}

	results := make(chan *execute.ExecutionResult, 10)

	err = kernel.client.ExecuteCodeStreamContext(streamCtx, kernel.kernelID, request.Code, results)
	if err != nil {
		return err
	}

	var executionError *execute.ErrorOutput
	for {
		select {
		case result, ok := <-results:
			if !ok || result == nil {
				if ctx.Err() == nil {
					if executionError == nil {
						executionError = &execute.ErrorOutput{EName: "ExecutionInterrupted", EValue: "Jupyter stream closed before execution completed"}
					}
					cleanup()
					request.Hooks.OnExecuteError(executionError)
					return errors.New("jupyter stream closed before execution completed")
				}
				// Cancellation can close the producer before this select observes Done.
				break
			}
			if result.Error != nil {
				executionError = result.Error
			}
			// Jupyter can report errors before the final count and output. Hold the
			// terminal hook until reply+idle, while forwarding all intervening data.
			output := *result
			output.Error = nil
			output.ExecutionTime = 0
			dispatchExecutionResultHooks(request, &output)
			if result.ExecutionTime > 0 {
				// Clients may reuse this session as soon as they see a terminal event.
				// Release this execution before exposing that event, exactly once.
				cleanup()
				dispatchExecutionResultHooks(request, &execute.ExecutionResult{
					Error: executionError, ExecutionTime: result.ExecutionTime,
				})
				return nil
			}
			continue

		case <-ctx.Done():
		}
		cancelStream()
		log.Warn("jupyter: context cancelled; interrupting kernel")
		err = kernel.client.InterruptKernel(kernel.kernelID)
		if err != nil {
			log.Error("jupyter: interrupt kernel: %v", err)
		}

		cleanup()
		request.Hooks.OnExecuteError(&execute.ErrorOutput{
			EName:  "ContextCancelled",
			EValue: "Interrupt kernel",
		})
		return errors.New("context cancelled, interrupt kernel")
	}
}

func dispatchExecutionResultHooks(request *ExecuteCodeRequest, result *execute.ExecutionResult) {
	if result.ExecutionCount > 0 || len(result.ExecutionData) > 0 {
		request.Hooks.OnExecuteResult(result.ExecutionData, result.ExecutionCount)
	}

	if result.Status != "" {
		request.Hooks.OnExecuteStatus(result.Status)
	}

	for _, stream := range result.Stream {
		switch stream.Name {
		case execute.StreamStdout:
			request.Hooks.OnExecuteStdout(stream.Text)
		case execute.StreamStderr:
			request.Hooks.OnExecuteStderr(stream.Text)
		}
	}

	if result.Error != nil {
		request.Hooks.OnExecuteError(result.Error)
	}

	// Emit terminal hooks only after all output. Completion is success-only.
	if result.ExecutionTime > 0 && result.Error == nil {
		request.Hooks.OnExecuteComplete(result.ExecutionTime)
	}
}

func (c *Controller) getJupyterKernel(sessionID string) *jupyterKernel {
	if v, ok := c.jupyterClientMap.Load(sessionID); ok {
		if kernel, ok := v.(*jupyterKernel); ok {
			return kernel
		}
	}
	return nil
}

// searchKernel finds a kernel spec name for the given language.
func (c *Controller) searchKernel(client *jupyter.Client, language Language) (string, error) {
	specs, err := client.GetKernelSpecs()
	if err != nil {
		return "", err
	}

	if len(specs.Kernelspecs) == 0 {
		return "", errors.New("no kernel specs found")
	}

	var matchingNames []string
	var fallback string
	for name, spec := range specs.Kernelspecs {
		if spec.Spec.Language != language.String() {
			continue
		}
		if name == "python3" {
			fallback = name
			continue
		}
		matchingNames = append(matchingNames, name)
	}
	if len(matchingNames) > 0 {
		sort.Strings(matchingNames)
		return matchingNames[0], nil
	}
	if fallback != "" {
		return fallback, nil
	}
	return "", errors.New("no kernel specs found")
}
