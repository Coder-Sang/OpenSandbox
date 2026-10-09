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

// Package execute provides functionality for executing Jupyter kernel code via WebSocket
package execute

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"sync"
	"time"

	"github.com/alibaba/opensandbox/internal/safego"
	"github.com/google/uuid"
	"github.com/gorilla/websocket"
)

type HTTPClient interface {
	Do(req *http.Request) (*http.Response, error)
}

type Client struct {
	httpClient HTTPClient

	conn           *websocket.Conn
	connectionDone chan struct{}

	handlers map[MessageType]func(*Message)

	session string

	msgCounter int

	mu      sync.Mutex
	writeMu sync.Mutex

	wsURL string
}

func NewClient(baseURL string, httpClient HTTPClient) *Client {
	return &Client{
		httpClient: httpClient,
		handlers:   make(map[MessageType]func(*Message)),
		session:    uuid.New().String(),
		msgCounter: 0,
	}
}

func (c *Client) Connect(wsURL string) error {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.wsURL = wsURL

	conn, resp, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if resp != nil && err != nil {
		resp.Body.Close()
	}
	if err != nil {
		return fmt.Errorf("failed to connect to kernel: %w", err)
	}
	if c.conn != nil {
		c.conn.Close()
		close(c.connectionDone)
	}
	c.conn = conn
	c.connectionDone = make(chan struct{})
	done := c.connectionDone
	c.handlers = make(map[MessageType]func(*Message))

	c.registerDefaultHandlers()

	safego.Go(func() { c.receiveConnectionMessages(conn, done) })

	return nil
}

func (c *Client) Disconnect() {
	c.mu.Lock()
	defer c.mu.Unlock()

	if c.conn != nil {
		c.conn.Close()
		close(c.connectionDone)
		c.conn = nil
	}
}

func (c *Client) IsConnected() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.conn != nil
}

type streamExecutionState struct {
	startTime    time.Time
	result       *ExecutionResult
	executeDone  bool
	executeMutex sync.Mutex
	resultMutex  sync.Mutex

	// chanMutex serializes channel closure against sends. Sends select on context
	// and connection cancellation, allowing teardown even when the consumer stops.
	chanMutex      sync.RWMutex
	closed         bool
	ctx            context.Context
	connectionDone <-chan struct{}
	finished       chan struct{}
	replySeen      bool
	idleSeen       bool
}

func newStreamExecutionState(startTime time.Time) *streamExecutionState {
	return &streamExecutionState{
		startTime: startTime,
		ctx:       context.Background(),
		finished:  make(chan struct{}),
		result: &ExecutionResult{
			Status:        "ok",
			Stream:        make([]*StreamOutput, 0),
			ExecutionTime: 0,
		},
	}
}

// trySend delivers notify on resultChan unless the channel has already been closed. It must be
// used for every send on resultChan so sends can never race closeResultChan.
func (state *streamExecutionState) trySend(resultChan chan *ExecutionResult, notify *ExecutionResult) {
	state.chanMutex.RLock()
	defer state.chanMutex.RUnlock()
	if state.closed || state.ctx.Err() != nil {
		return
	}
	select {
	case <-state.connectionDone:
		return
	default:
	}
	select {
	case resultChan <- notify:
	case <-state.ctx.Done():
	case <-state.connectionDone:
	case <-state.finished:
	}
}

// closeResultChan waits for in-flight cancellable sends and closes exactly once.
func (state *streamExecutionState) closeResultChan(resultChan chan *ExecutionResult) {
	state.chanMutex.Lock()
	defer state.chanMutex.Unlock()
	if !state.closed {
		state.closed = true
		close(state.finished)
		close(resultChan)
	}
}

func (c *Client) ExecuteCodeStream(code string, resultChan chan *ExecutionResult) error {
	return c.ExecuteCodeStreamContext(context.Background(), code, resultChan)
}

// ExecuteCodeStreamContext owns resultChan once request handlers are installed.
// Setup errors before installation leave the channel untouched. Write errors,
// cancellation and disconnect unblock output delivery and close the channel.
func (c *Client) ExecuteCodeStreamContext(ctx context.Context, code string, resultChan chan *ExecutionResult) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	c.mu.Lock()
	conn, done := c.conn, c.connectionDone
	c.mu.Unlock()
	if conn == nil {
		return errors.New("not connected to kernel, please call Connect method")
	}

	msg, err := c.buildExecuteMessage(code)
	if err != nil {
		return err
	}

	ctx, cancel := context.WithCancel(ctx)
	state := newStreamExecutionState(time.Now())
	state.ctx = ctx
	state.connectionDone = done

	c.clearTemporaryHandlers()
	c.registerExecuteCodeStreamHandlers(state, resultChan, msg.Header.MessageID)
	safego.Go(func() {
		select {
		case <-ctx.Done():
			conn.Close()
		case <-done:
		case <-state.finished:
			cancel()
			return
		}
		state.closeResultChan(resultChan)
		cancel()
	})

	c.writeMu.Lock()
	c.mu.Lock()
	current := c.conn == conn
	c.mu.Unlock()
	if !current {
		err = errors.New("kernel connection changed")
	} else if ctx.Err() != nil {
		err = ctx.Err()
	} else {
		if deadline, ok := ctx.Deadline(); ok {
			conn.SetWriteDeadline(deadline)
		} else {
			conn.SetWriteDeadline(time.Time{})
		}
		err = conn.WriteJSON(msg)
	}
	c.writeMu.Unlock()
	if err != nil {
		cancel()
		state.closeResultChan(resultChan)
		return fmt.Errorf("failed to send execution request: %w", err)
	}

	return nil
}

func (c *Client) buildExecuteMessage(code string) (*Message, error) {
	msgID := c.nextMessageID()
	request := &ExecuteRequest{
		Code:            code,
		Silent:          false,
		StoreHistory:    true,
		UserExpressions: make(map[string]string),
		AllowStdin:      false,
		StopOnError:     true,
	}

	content, err := json.Marshal(request)
	if err != nil {
		return nil, fmt.Errorf("failed to serialize request: %w", err)
	}

	msg := &Message{
		Header: Header{
			MessageID:   msgID,
			Username:    "go-client",
			Session:     c.session,
			Date:        time.Now().Format(time.RFC3339),
			MessageType: string(MsgExecuteRequest),
			Version:     "5.3",
		},
		ParentHeader: Header{},
		Metadata:     make(map[string]interface{}),
		Content:      content,
		Channel:      "shell",
	}

	return msg, nil
}

func (c *Client) registerExecuteCodeStreamHandlers(state *streamExecutionState, resultChan chan *ExecutionResult, requestID string) {
	register := func(kind MessageType, handler func(*Message)) {
		c.registerHandler(kind, func(msg *Message) {
			if msg.ParentHeader.MessageID == requestID {
				handler(msg)
			}
		})
	}
	register(MsgDisplayData, func(msg *Message) {
		var display DisplayData
		if json.Unmarshal(msg.Content, &display) == nil {
			state.trySend(resultChan, &ExecutionResult{ExecutionData: display.Data})
		}
	})
	register(MsgExecuteReply, func(msg *Message) {
		state.resultMutex.Lock()
		alreadyErrored := state.result.Error != nil
		state.resultMutex.Unlock()
		c.handleExecuteReply(msg, state)
		if !alreadyErrored {
			var reply ExecuteReply
			if json.Unmarshal(msg.Content, &reply) == nil && reply.EName != "" {
				state.trySend(resultChan, &ExecutionResult{Status: "error", Error: &reply.ErrorOutput})
			}
		}
		c.maybeFinalizeExecution(state, resultChan)
	})
	register(MsgExecuteResult, func(msg *Message) {
		c.handleExecuteResult(msg, state, resultChan)
	})
	register(MsgStream, func(msg *Message) {
		c.handleStreamOutput(msg, state, resultChan)
	})
	register(MsgError, func(msg *Message) {
		c.handleExecutionError(msg, state, resultChan)
	})
	register(MsgStatus, func(msg *Message) {
		c.handleExecutionStatus(msg, state, resultChan)
	})
}

func (c *Client) handleExecuteReply(msg *Message, state *streamExecutionState) {
	var execReply ExecuteReply
	if err := json.Unmarshal(msg.Content, &execReply); err != nil {
		return
	}

	state.resultMutex.Lock()
	defer state.resultMutex.Unlock()
	state.replySeen = true
	state.result.ExecutionCount = execReply.ExecutionCount
	state.result.Status = execReply.Status
	if execReply.EName != "" {
		state.result.Error = &execReply.ErrorOutput
	}
}

func (c *Client) handleExecuteResult(msg *Message, state *streamExecutionState, resultChan chan *ExecutionResult) {
	var execResult ExecuteResult
	if err := json.Unmarshal(msg.Content, &execResult); err != nil {
		return
	}

	// resultChan send happens outside resultMutex: sending can block indefinitely on a full,
	// undrained channel, and holding resultMutex across that block would starve any other
	// goroutine that needs it -- including finalizeExecution's poll loop. trySend (see
	// streamExecutionState) separately makes this send race-free against finalizeExecution
	// closing resultChan, since ExecutionCount/Error can also be set by handleExecuteReply with
	// no send of its own.
	notify := &ExecutionResult{
		ExecutionCount: execResult.ExecutionCount,
		ExecutionData:  execResult.Data,
	}
	state.trySend(resultChan, notify)

	state.resultMutex.Lock()
	state.result.ExecutionCount = execResult.ExecutionCount
	state.resultMutex.Unlock()
}

func (c *Client) handleStreamOutput(msg *Message, state *streamExecutionState, resultChan chan *ExecutionResult) {
	var stream StreamOutput
	if err := json.Unmarshal(msg.Content, &stream); err != nil {
		return
	}

	// See handleExecuteResult: resultChan send must stay outside the lock.
	state.resultMutex.Lock()
	state.result.Stream = append(state.result.Stream, &stream)
	state.resultMutex.Unlock()

	notify := &ExecutionResult{
		Stream: []*StreamOutput{&stream},
	}
	state.trySend(resultChan, notify)
}

func (c *Client) handleExecutionError(msg *Message, state *streamExecutionState, resultChan chan *ExecutionResult) {
	var errOutput ErrorOutput
	if err := json.Unmarshal(msg.Content, &errOutput); err != nil {
		return
	}

	// See handleExecuteResult: resultChan send must stay outside resultMutex, and goes through
	// trySend so it can't race finalizeExecution closing resultChan.
	notify := &ExecutionResult{
		Error:  &errOutput,
		Status: "error",
	}
	state.trySend(resultChan, notify)

	state.resultMutex.Lock()
	state.result.Status = "error"
	state.result.Error = &errOutput
	state.resultMutex.Unlock()
}

func (c *Client) handleExecutionStatus(msg *Message, state *streamExecutionState, resultChan chan *ExecutionResult) {
	var status StatusUpdate
	if err := json.Unmarshal(msg.Content, &status); err != nil {
		return
	}
	if status.ExecutionState != StateIdle {
		return
	}

	state.resultMutex.Lock()
	state.idleSeen = true
	state.resultMutex.Unlock()
	c.maybeFinalizeExecution(state, resultChan)
}

func (c *Client) maybeFinalizeExecution(state *streamExecutionState, resultChan chan *ExecutionResult) {
	state.resultMutex.Lock()
	ready := state.replySeen && state.idleSeen
	state.resultMutex.Unlock()
	if !ready {
		return
	}
	state.executeMutex.Lock()
	defer state.executeMutex.Unlock()
	if state.executeDone {
		return
	}
	state.executeDone = true
	c.finalizeExecution(state, resultChan)
}

func (c *Client) finalizeExecution(state *streamExecutionState, resultChan chan *ExecutionResult) {
	// See handleExecuteResult: resultChan send must stay outside resultMutex.
	state.resultMutex.Lock()
	state.result.ExecutionTime = time.Since(state.startTime)
	notify := &ExecutionResult{
		ExecutionTime:  state.result.ExecutionTime,
		ExecutionCount: state.result.ExecutionCount,
		Status:         state.result.Status,
	}
	state.resultMutex.Unlock()
	state.trySend(resultChan, notify)

	// closeResultChan waits for any send from handleExecuteResult/handleStreamOutput/
	// handleExecutionError still in flight to finish before closing, and marks resultChan closed
	// so any later, straggling message for this execution (e.g. output from a background thread
	// that outlives execute_reply) is dropped by trySend instead of racing this close.
	state.closeResultChan(resultChan)
}

func (c *Client) writeMessage(msg *Message) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	c.mu.Lock()
	conn := c.conn
	c.mu.Unlock()
	if conn == nil {
		return errors.New("not connected to kernel")
	}
	if err := conn.SetWriteDeadline(time.Time{}); err != nil {
		return err
	}
	return conn.WriteJSON(msg)
}

func (c *Client) ExecuteCodeWithCallback(code string, handler CallbackHandler) error {
	if !c.IsConnected() {
		return errors.New("not connected to kernel, please call Connect method")
	}

	msgID := c.nextMessageID()
	request := &ExecuteRequest{
		Code:            code,
		Silent:          false,
		StoreHistory:    true,
		UserExpressions: make(map[string]string),
		AllowStdin:      false,
		StopOnError:     true,
	}

	content, err := json.Marshal(request)
	if err != nil {
		return fmt.Errorf("failed to serialize request: %w", err)
	}

	msg := &Message{
		Header: Header{
			MessageID:   msgID,
			Username:    "go-client",
			Session:     c.session,
			Date:        time.Now().Format(time.RFC3339),
			MessageType: string(MsgExecuteRequest),
			Version:     "5.3",
		},
		ParentHeader: Header{},
		Metadata:     make(map[string]interface{}),
		Content:      content,
		Channel:      "shell",
	}

	c.clearTemporaryHandlers()
	register := func(kind MessageType, callback func(*Message)) {
		c.registerHandler(kind, func(response *Message) {
			if response.ParentHeader.MessageID == msgID {
				callback(response)
			}
		})
	}
	if handler.OnExecuteResult != nil {
		register(MsgExecuteResult, func(msg *Message) {
			var execResult ExecuteResult
			if err := json.Unmarshal(msg.Content, &execResult); err != nil {
				return
			}
			handler.OnExecuteResult(&execResult)
		})
	}

	if handler.OnStream != nil {
		register(MsgStream, func(msg *Message) {
			var stream StreamOutput
			if err := json.Unmarshal(msg.Content, &stream); err != nil {
				return
			}
			handler.OnStream(&stream)
		})
	}

	if handler.OnDisplayData != nil {
		register(MsgDisplayData, func(msg *Message) {
			var display DisplayData
			if err := json.Unmarshal(msg.Content, &display); err != nil {
				return
			}
			handler.OnDisplayData(&display)
		})
	}

	if handler.OnError != nil {
		register(MsgError, func(msg *Message) {
			var errOutput ErrorOutput
			if err := json.Unmarshal(msg.Content, &errOutput); err != nil {
				return
			}
			handler.OnError(&errOutput)
		})
	}

	if handler.OnStatus != nil {
		register(MsgStatus, func(msg *Message) {
			var status StatusUpdate
			if err := json.Unmarshal(msg.Content, &status); err != nil {
				return
			}
			handler.OnStatus(&status)
		})
	}

	err = c.writeMessage(msg)
	if err != nil {
		return fmt.Errorf("failed to send execution request: %w", err)
	}

	return nil
}

func (c *Client) registerDefaultHandlers() {}

func (c *Client) registerHandler(msgType MessageType, handler func(*Message)) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.handlers[msgType] = handler
}

func (c *Client) clearTemporaryHandlers() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.handlers = make(map[MessageType]func(*Message))
	c.registerDefaultHandlers()
}

func (c *Client) receiveConnectionMessages(conn *websocket.Conn, done chan struct{}) {
	defer func() {
		c.mu.Lock()
		defer c.mu.Unlock()
		if c.conn == conn {
			conn.Close()
			c.conn = nil
			close(done)
		}
	}()
	for {
		var msg Message
		if conn.ReadJSON(&msg) != nil {
			return
		}
		c.mu.Lock()
		if c.conn != conn {
			c.mu.Unlock()
			return
		}
		handler := c.handlers[MessageType(msg.Header.MessageType)]
		c.mu.Unlock()
		if handler != nil {
			handler(&msg)
		}
	}
}

func (c *Client) handleMessage(msg *Message) {
	msgType := MessageType(msg.Header.MessageType)

	c.mu.Lock()
	handler, ok := c.handlers[msgType]
	c.mu.Unlock()

	if ok && handler != nil {
		handler(msg)
	}
}

func (c *Client) nextMessageID() string {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.msgCounter++
	return fmt.Sprintf("%s-%d", c.session, c.msgCounter)
}
