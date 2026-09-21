// Copyright 2026 Alibaba Group Holding Ltd.
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

package main

import (
	"context"
	"errors"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/alibaba/opensandbox/execd/pkg/isolation"
	"github.com/alibaba/opensandbox/execd/pkg/lifecycle"
	"github.com/alibaba/opensandbox/execd/pkg/runtime"
)

func TestNormalizePoolIsolationConfigPreservesOnlyNarrowOptIns(t *testing.T) {
	cfg := isolation.DefaultConfig()
	cfg.Hardening = &isolation.HardeningConfig{
		Enabled:                      false,
		KeepCapabilities:             []string{"CAP_SYS_ADMIN"},
		AllowSeccompUserNotification: true,
	}
	cfg.Landlock = &isolation.LandlockConfig{
		Enabled:              false,
		AllowPrivateProcRead: true,
		ExtraReadable:        []string{"/secret"},
		ExtraWritable:        []string{"/"},
	}
	cfg.Seccomp = &isolation.SeccompOverride{Deny: []string{"getpid"}}

	got := normalizePoolIsolationConfig(cfg)
	if got.Hardening == nil || !got.Hardening.Enabled || !got.Hardening.AllowSeccompUserNotification {
		t.Fatalf("normalized hardening = %#v", got.Hardening)
	}
	if len(got.Hardening.KeepCapabilities) != 0 {
		t.Fatalf("normalized keep capabilities = %v", got.Hardening.KeepCapabilities)
	}
	if got.Landlock == nil || !got.Landlock.Enabled || !got.Landlock.AllowPrivateProcRead {
		t.Fatalf("normalized landlock = %#v", got.Landlock)
	}
	if len(got.Landlock.ExtraReadable) != 0 || len(got.Landlock.ExtraWritable) != 0 {
		t.Fatalf("normalized Landlock path grants leaked: %#v", got.Landlock)
	}
	if got.Seccomp == nil {
		t.Fatal("nested seccomp opt-in must derive a mandatory Pool denylist")
	}
	for _, name := range got.Seccomp.Deny {
		if name == "seccomp" || name == "getpid" {
			t.Fatalf("normalized denylist contains unexpected syscall %q: %v", name, got.Seccomp.Deny)
		}
	}
}

func TestValidatePrivateProcReadMode(t *testing.T) {
	cfg := isolation.DefaultConfig()
	cfg.Landlock = &isolation.LandlockConfig{Enabled: true, AllowPrivateProcRead: true}
	if err := validatePrivateProcReadMode(cfg, false); err == nil {
		t.Fatal("private proc read outside forced Pool isolation must fail")
	}
	if err := validatePrivateProcReadMode(cfg, true); err != nil {
		t.Fatalf("private proc read in forced Pool isolation failed: %v", err)
	}
	cfg.Landlock.AllowPrivateProcRead = false
	if err := validatePrivateProcReadMode(cfg, false); err != nil {
		t.Fatalf("default private proc policy failed: %v", err)
	}
}

type fakeIsolatedRunnerCloser struct {
	closeFn func() error
}

func TestStartLifecycleReportsStartupStatus(t *testing.T) {
	tests := []struct {
		name           string
		timeoutSeconds int
		helperResult   string
		wantStatus     string
		wantError      bool
	}{
		{name: "default timeout", helperResult: "success", wantStatus: "running 60\ndone 0\n"},
		{name: "hook failure", timeoutSeconds: 2, helperResult: "failure", wantStatus: "running 2\ndone 1\n", wantError: true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			statusFile := filepath.Join(t.TempDir(), "lifecycle-status")
			if err := os.WriteFile(statusFile, nil, 0o600); err != nil {
				t.Fatal(err)
			}
			cfg := &lifecycle.Config{PreStart: &lifecycle.Hook{
				Command: []string{
					os.Args[0], "-test.run=^TestLifecycleStartupCommandHelper$", "--", test.helperResult,
				},
				TimeoutSeconds: test.timeoutSeconds,
			}}

			manager, err := startLifecycle(context.Background(), cfg, statusFile)
			if (err != nil) != test.wantError {
				t.Fatalf("startLifecycle() error = %v, wantError %v", err, test.wantError)
			}
			if manager != nil {
				manager.Stop()
			}
			raw, err := os.ReadFile(statusFile)
			if err != nil {
				t.Fatal(err)
			}
			if got := string(raw); got != test.wantStatus {
				t.Fatalf("lifecycle status = %q, want %q", got, test.wantStatus)
			}
		})
	}
}

func TestLifecycleStartupCommandHelper(*testing.T) {
	switch os.Args[len(os.Args)-1] {
	case "failure":
		os.Exit(2)
	case "wait":
		time.Sleep(time.Hour)
	}
}

func TestStartLifecycleCancellationStatus(t *testing.T) {
	for _, test := range []struct {
		name             string
		removeStatusFile bool
	}{
		{name: "reported shutdown"},
		{name: "status failure", removeStatusFile: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			statusFile := filepath.Join(t.TempDir(), "lifecycle-status")
			if err := os.WriteFile(statusFile, nil, 0o600); err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			cfg := &lifecycle.Config{PreStart: &lifecycle.Hook{Command: []string{
				os.Args[0], "-test.run=^TestLifecycleStartupCommandHelper$", "--", "wait",
			}}}
			result := make(chan error, 1)
			go func() {
				_, err := startLifecycle(ctx, cfg, statusFile)
				result <- err
			}()

			deadline := time.After(2 * time.Second)
			for {
				raw, err := os.ReadFile(statusFile)
				if err != nil {
					t.Fatal(err)
				}
				if len(raw) > 0 {
					break
				}
				select {
				case <-deadline:
					t.Fatal("preStart did not report running status")
				case <-time.After(10 * time.Millisecond):
				}
			}
			if test.removeStatusFile {
				if err := os.Remove(statusFile); err != nil {
					t.Fatal(err)
				}
			}
			cancel()
			select {
			case err := <-result:
				if test.removeStatusFile {
					if errors.Is(err, errStartupShutdown) || !errors.Is(err, os.ErrNotExist) {
						t.Fatalf("startLifecycle() error = %v, want status-file failure", err)
					}
				} else if !errors.Is(err, errStartupShutdown) {
					t.Fatalf("startLifecycle() error = %v, want %v", err, errStartupShutdown)
				}
			case <-time.After(2 * time.Second):
				t.Fatal("startLifecycle did not return after cancellation")
			}
		})
	}
}

func (f *fakeIsolatedRunnerCloser) Close() error {
	return f.closeFn()
}

func TestCloseIsolatedRunnerRetriesRetainedNamespaceOwnership(t *testing.T) {
	busyErr := errors.New("namespace pin is busy")
	closeCalls := 0
	var reported []error
	runner := &fakeIsolatedRunnerCloser{
		closeFn: func() error {
			closeCalls++
			if closeCalls == 1 {
				return errors.Join(
					runtime.ErrSessionNamespaceCleanup,
					busyErr,
				)
			}
			return nil
		},
	}

	if err := closeIsolatedRunnerWithRetry(
		runner,
		time.Second,
		time.Millisecond,
		func(err error) {
			reported = append(reported, err)
		},
	); err != nil {
		t.Fatal(err)
	}
	if closeCalls != 2 {
		t.Fatalf("Close calls = %d, want 2", closeCalls)
	}
	if len(reported) != 1 ||
		!errors.Is(reported[0], runtime.ErrSessionNamespaceCleanup) ||
		!errors.Is(reported[0], busyErr) {
		t.Fatalf("reported retry errors = %v", reported)
	}
}

func TestCloseIsolatedRunnerStopsAtRetryDeadline(t *testing.T) {
	busyErr := errors.New("namespace pin is permanently busy")
	closeCalls := 0
	runner := &fakeIsolatedRunnerCloser{
		closeFn: func() error {
			closeCalls++
			return errors.Join(
				runtime.ErrSessionNamespaceCleanup,
				busyErr,
			)
		},
	}

	err := closeIsolatedRunnerWithRetry(
		runner,
		10*time.Millisecond,
		time.Hour,
		nil,
	)
	if !errors.Is(err, runtime.ErrSessionNamespaceCleanup) ||
		!errors.Is(err, busyErr) ||
		!errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("close error = %v", err)
	}
	if closeCalls != 1 {
		t.Fatalf("Close calls = %d, want 1", closeCalls)
	}
}

func TestCloseIsolatedRunnerDoesNotRetryTerminalError(t *testing.T) {
	closeErr := errors.New("terminal cleanup error")
	closeCalls := 0
	runner := &fakeIsolatedRunnerCloser{
		closeFn: func() error {
			closeCalls++
			return closeErr
		},
	}

	err := closeIsolatedRunnerWithRetry(
		runner,
		time.Second,
		time.Millisecond,
		nil,
	)
	if !errors.Is(err, closeErr) {
		t.Fatalf("close error = %v, want %v", err, closeErr)
	}
	if closeCalls != 1 {
		t.Fatalf("Close calls = %d, want 1", closeCalls)
	}
}

func TestCloseIsolatedRunnerDoesNotRetryTeardownTimeout(t *testing.T) {
	closeCalls := 0
	runner := &fakeIsolatedRunnerCloser{
		closeFn: func() error {
			closeCalls++
			return runtime.ErrSessionTeardownTimeout
		},
	}

	err := closeIsolatedRunnerWithRetry(
		runner,
		time.Second,
		time.Millisecond,
		nil,
	)
	if !errors.Is(err, runtime.ErrSessionTeardownTimeout) {
		t.Fatalf(
			"close error = %v, want %v",
			err,
			runtime.ErrSessionTeardownTimeout,
		)
	}
	if closeCalls != 1 {
		t.Fatalf("Close calls = %d, want 1", closeCalls)
	}
}

func TestServeHTTPUntilShutdownReturnsAfterContextCancellation(t *testing.T) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	serveDone := make(chan error, 1)
	go func() {
		serveDone <- serveHTTPUntilShutdown(
			ctx,
			listener,
			http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				w.WriteHeader(http.StatusNoContent)
			}),
			func() error { return nil },
		)
	}()

	response, err := http.Get("http://" + listener.Addr().String())
	if err != nil {
		cancel()
		t.Fatal(err)
	}
	_, _ = io.Copy(io.Discard, response.Body)
	_ = response.Body.Close()
	cancel()

	select {
	case err := <-serveDone:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("HTTP server did not stop after shutdown cancellation")
	}
}

func TestServeHTTPUntilShutdownOnFatalReturnsFatalError(t *testing.T) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	fatal := make(chan error, 1)
	serveDone := make(chan error, 1)
	go func() {
		serveDone <- serveHTTPUntilShutdownOnFatal(
			ctx,
			listener,
			http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				w.WriteHeader(http.StatusNoContent)
			}),
			func() error { return nil },
			fatal,
		)
	}()

	response, err := http.Get("http://" + listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	wantErr := errors.New("pool runtime exited")
	fatal <- wantErr

	select {
	case err := <-serveDone:
		if !errors.Is(err, wantErr) {
			t.Fatalf("server error = %v, want %v", err, wantErr)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("HTTP server did not stop after fatal runtime error")
	}
}

func TestServeHTTPUntilShutdownServesDuringStartup(t *testing.T) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	startupStarted := make(chan struct{})
	finishStartup := make(chan struct{})
	serveDone := make(chan error, 1)
	go func() {
		serveDone <- serveHTTPUntilShutdown(
			ctx,
			listener,
			http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				w.WriteHeader(http.StatusNoContent)
			}),
			func() error {
				close(startupStarted)
				<-finishStartup
				return nil
			},
		)
	}()

	<-startupStarted
	response, err := http.Get("http://" + listener.Addr().String())
	if err != nil {
		close(finishStartup)
		cancel()
		t.Fatal(err)
	}
	_ = response.Body.Close()
	if response.StatusCode != http.StatusNoContent {
		t.Fatalf("status = %d, want %d", response.StatusCode, http.StatusNoContent)
	}

	close(finishStartup)
	cancel()
	select {
	case err := <-serveDone:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("HTTP server did not stop after startup completed")
	}
}
