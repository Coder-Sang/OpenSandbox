//go:build linux

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

package runtime

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"golang.org/x/sys/unix"

	"github.com/alibaba/opensandbox/execd/pkg/isolation"
	"github.com/alibaba/opensandbox/execd/pkg/log"
)

const poolRuntimeStartupTimeout = 10 * time.Second

// PoolMountRoot is a trusted source declared by the Pool template.
type PoolMountRoot struct {
	MountRoot      string   `json:"mountRoot"`
	Source         string   `json:"source"`
	TargetPrefixes []string `json:"targetPrefixes"`
	MaxMode        string   `json:"maxMode"`
}

// PoolMountSelector is the request-time selection within a trusted root.
type PoolMountSelector struct {
	Root    string `json:"root"`
	SubPath string `json:"subPath"`
	Target  string `json:"target"`
	Mode    string `json:"mode"`
}

// PoolIsolationSpec is the complete, server-validated bwrap-v1 binding. Execd
// validates it again before it consumes the one-shot runtime-init slot.
type PoolIsolationSpec struct {
	Type   string                   `json:"type"`
	Roots  map[string]PoolMountRoot `json:"roots"`
	Mounts []PoolMountSelector      `json:"mounts"`
}

type poolRuntimeState struct {
	pid        int
	root       *os.File
	namespaces []*os.File
	helper     string
	process    *managedProcess
	lifecycle  isolation.WorkloadLifecycle
	valid      atomic.Bool
}

var activePoolRuntime atomic.Pointer[poolRuntimeState]
var poolRuntimeRequired atomic.Bool

// PoolRuntimeManager owns the single long-lived bwrap runtime for a pooled
// Pod. It is intentionally one-shot: a failed or exited runtime is never
// replaced in the same Pod.
type PoolRuntimeManager struct {
	isolator isolation.LifecycleIsolator
	mu       sync.Mutex
	started  bool
	state    *poolRuntimeState
}

// NewPoolRuntimeManager creates a manager. A nil isolator is retained so a
// forced bwrap request fails closed instead of falling back to direct exec.
func NewPoolRuntimeManager(iso isolation.LifecycleIsolator) *PoolRuntimeManager {
	return &PoolRuntimeManager{isolator: iso}
}

// ValidatePoolIsolation validates policy and selector structure without
// touching the filesystem.
func ValidatePoolIsolation(spec *PoolIsolationSpec) error {
	if spec == nil || spec.Type != "bwrap" {
		return errors.New("isolation.type must be bwrap")
	}
	if len(spec.Roots) == 0 {
		return errors.New("isolation roots must not be empty")
	}
	for name, root := range spec.Roots {
		if strings.TrimSpace(name) == "" {
			return errors.New("isolation root name must not be blank")
		}
		if !cleanAbsolute(root.MountRoot) || !cleanAbsolute(root.Source) {
			return fmt.Errorf("isolation root %q paths must be normalized absolute paths", name)
		}
		if !pathWithin(root.MountRoot, root.Source) {
			return fmt.Errorf("isolation root %q source must be beneath mountRoot", name)
		}
		if root.MountRoot == "/" {
			return fmt.Errorf("isolation root %q mountRoot must not be the filesystem root", name)
		}
		for _, control := range []string{"/opt/opensandbox", "/run/execd", "/proc", "/sys", "/dev", "/var/run/secrets"} {
			if pathWithin(control, root.MountRoot) || pathWithin(root.MountRoot, control) {
				return fmt.Errorf("isolation root %q mountRoot overlaps control path %q", name, control)
			}
		}
		if root.MaxMode != "ro" && root.MaxMode != "rw" {
			return fmt.Errorf("isolation root %q maxMode must be ro or rw", name)
		}
		if len(root.TargetPrefixes) == 0 {
			return fmt.Errorf("isolation root %q targetPrefixes must not be empty", name)
		}
		for _, prefix := range root.TargetPrefixes {
			if !cleanAbsolute(prefix) || prefix == "/" {
				return fmt.Errorf("isolation root %q has invalid target prefix %q", name, prefix)
			}
		}
	}

	targets := make([]string, 0, len(spec.Mounts))
	controlPaths := []string{
		"/opt/opensandbox", "/run/execd", "/proc", "/sys", "/dev", "/var/run/secrets",
	}
	for i, mount := range spec.Mounts {
		root, ok := spec.Roots[mount.Root]
		if !ok {
			return fmt.Errorf("isolation mount %d references unknown root %q", i, mount.Root)
		}
		if mount.SubPath == "" || filepath.IsAbs(mount.SubPath) || filepath.Clean(mount.SubPath) != mount.SubPath || mount.SubPath == ".." || strings.HasPrefix(mount.SubPath, ".."+string(os.PathSeparator)) {
			return fmt.Errorf("isolation mount %d subPath must be a normalized relative path", i)
		}
		if !cleanAbsolute(mount.Target) || mount.Target == "/" {
			return fmt.Errorf("isolation mount %d target must be a normalized absolute path", i)
		}
		allowed := false
		for _, prefix := range root.TargetPrefixes {
			if pathWithin(prefix, mount.Target) {
				allowed = true
				break
			}
		}
		if !allowed {
			return fmt.Errorf("isolation mount %d target is outside the root policy", i)
		}
		for _, control := range controlPaths {
			if pathWithin(control, mount.Target) || pathWithin(mount.Target, control) {
				return fmt.Errorf("isolation mount %d target overlaps control path %q", i, control)
			}
		}
		for _, declaredRoot := range spec.Roots {
			if pathWithin(declaredRoot.MountRoot, mount.Target) || pathWithin(mount.Target, declaredRoot.MountRoot) {
				return fmt.Errorf("isolation mount %d target overlaps trusted mount root", i)
			}
		}
		if mount.Mode != "ro" && mount.Mode != "rw" {
			return fmt.Errorf("isolation mount %d mode must be ro or rw", i)
		}
		if root.MaxMode == "ro" && mount.Mode == "rw" {
			return fmt.Errorf("isolation mount %d exceeds root maxMode", i)
		}
		for _, target := range targets {
			if pathWithin(target, mount.Target) || pathWithin(mount.Target, target) {
				return fmt.Errorf("isolation mount targets %q and %q overlap", target, mount.Target)
			}
		}
		targets = append(targets, mount.Target)
	}
	return nil
}

func cleanAbsolute(value string) bool {
	return filepath.IsAbs(value) && filepath.Clean(value) == value
}

func pathWithin(parent, child string) bool {
	rel, err := filepath.Rel(parent, child)
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(os.PathSeparator))
}

// Start creates and authenticates the long-lived bwrap runtime.
func (m *PoolRuntimeManager) Start(spec *PoolIsolationSpec) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.started {
		return errors.New("pool bwrap runtime already started")
	}
	m.started = true
	// From this point forward a missing runtime is an error, never a signal to
	// fall back to execd's direct execution path.
	poolRuntimeRequired.Store(true)
	if err := ValidatePoolIsolation(spec); err != nil {
		return err
	}
	if err := RequirePoolHardening(); err != nil {
		return err
	}
	if m.isolator == nil {
		return errors.New("pool bwrap runtime is unavailable")
	}

	binds, masks, err := openPoolMounts(spec)
	if err != nil {
		return err
	}
	closeBinds := true
	defer func() {
		if closeBinds {
			closePoolBinds(binds)
		}
	}()

	// The launcher is the only execd helper user commands need. Expose a
	// descriptor-pinned, read-only copy after /opt/opensandbox is masked.
	launcher, err := os.Open("/opt/opensandbox/opensandbox-launcher")
	if err != nil {
		return fmt.Errorf("open trusted runtime launcher: %w", err)
	}
	binds = append(binds, isolation.BindMount{
		SourceFile: launcher,
		Dest:       "/opt/opensandbox/opensandbox-launcher",
		ReadOnly:   true,
	})

	zero := uint32(0)
	opts := isolation.WrapOptions{
		Profile:          isolation.ProfileStrict,
		Binds:            binds,
		ShareNet:         true,
		Uid:              &zero,
		Gid:              &zero,
		UidMode:          isolation.UidModeUserns,
		RootWritable:     true,
		SkipWorkspace:    true,
		MaskPaths:        append(masks, "/opt/opensandbox", "/run/execd", "/var/run/secrets/kubernetes.io/serviceaccount"),
		DropCapabilities: true,
		EnvPassthrough:   isolation.EnvSpec{Mode: isolation.EnvModeDeny},
	}
	cmd := exec.Command("/bin/sh", "-c", "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done")
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	lifecycle, err := m.isolator.WrapWithLifecycle(cmd, opts)
	if err != nil {
		return fmt.Errorf("prepare pool bwrap runtime: %w", err)
	}
	if lifecycle == nil {
		return errors.New("pool bwrap runtime lifecycle is unavailable")
	}
	mp, err := launchManaged(cmd, withoutHardening(), withoutPoolRuntime())
	if err != nil {
		lifecycle.Abort()
		_ = lifecycle.Close()
		closeCommandExtraFiles(cmd)
		return fmt.Errorf("start pool bwrap runtime: %w", err)
	}
	closeCommandExtraFiles(cmd)
	closeBinds = false

	ctx, cancel := context.WithTimeout(context.Background(), poolRuntimeStartupTimeout)
	identity, err := lifecycle.WaitForIdentity(ctx)
	cancel()
	if err != nil {
		lifecycle.Abort()
		return fmt.Errorf("wait for pool bwrap identity: %w", err)
	}
	rootFD, err := unix.Open(fmt.Sprintf("/proc/%d/root", identity.PID), unix.O_PATH|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		lifecycle.Abort()
		return fmt.Errorf("pin pool bwrap root: %w", err)
	}
	root := os.NewFile(uintptr(rootFD), "pool-bwrap-root")
	namespaceFiles, err := pinPoolNamespaces(identity.PID)
	if err != nil {
		_ = root.Close()
		lifecycle.Abort()
		return err
	}
	helper, err := findPoolNsenter()
	if err != nil {
		_ = root.Close()
		closePoolFiles(namespaceFiles)
		lifecycle.Abort()
		return err
	}
	state := &poolRuntimeState{pid: identity.PID, root: root, namespaces: namespaceFiles, helper: helper, process: mp, lifecycle: lifecycle}
	state.valid.Store(true)
	if err := lifecycle.MarkReady(); err != nil {
		state.valid.Store(false)
		_ = root.Close()
		closePoolFiles(namespaceFiles)
		lifecycle.Abort()
		return fmt.Errorf("release pool bwrap gate: %w", err)
	}
	startupTimer := time.NewTimer(100 * time.Millisecond)
	select {
	case <-lifecycle.DrainDone():
		startupTimer.Stop()
		state.valid.Store(false)
		_ = root.Close()
		closePoolFiles(namespaceFiles)
		if err := lifecycle.DrainError(); err != nil {
			return fmt.Errorf("pool bwrap runtime exited during startup: %w", err)
		}
		return errors.New("pool bwrap runtime exited during startup")
	case <-startupTimer.C:
	}
	if err := syscall.Kill(cmd.Process.Pid, 0); err != nil {
		state.valid.Store(false)
		_ = root.Close()
		return fmt.Errorf("pool bwrap runtime failed startup probe: %w", err)
	}
	m.state = state
	activePoolRuntime.Store(state)
	go monitorPoolRuntime(state)
	log.Info("pool bwrap runtime ready pid=%d mounts=%d", identity.PID, len(spec.Mounts))
	return nil
}

func openPoolMounts(spec *PoolIsolationSpec) ([]isolation.BindMount, []string, error) {
	binds := make([]isolation.BindMount, 0, len(spec.Mounts))
	maskSet := make(map[string]struct{})
	for _, root := range spec.Roots {
		maskSet[root.MountRoot] = struct{}{}
	}
	for _, selector := range spec.Mounts {
		root := spec.Roots[selector.Root]
		mountRootFD, err := unix.Openat2(unix.AT_FDCWD, root.MountRoot, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
			Resolve: unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		if err != nil {
			closePoolBinds(binds)
			return nil, nil, fmt.Errorf("open isolation mountRoot %q: %w", selector.Root, err)
		}
		sourceRelative, err := filepath.Rel(root.MountRoot, root.Source)
		if err != nil {
			_ = unix.Close(mountRootFD)
			closePoolBinds(binds)
			return nil, nil, fmt.Errorf("resolve isolation root %q: %w", selector.Root, err)
		}
		rootFD, err := unix.Openat2(mountRootFD, sourceRelative, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
			Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		_ = unix.Close(mountRootFD)
		if err != nil {
			closePoolBinds(binds)
			return nil, nil, fmt.Errorf("open isolation root %q: %w", selector.Root, err)
		}
		fd, err := unix.Openat2(rootFD, selector.SubPath, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
			Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		_ = unix.Close(rootFD)
		if err != nil {
			closePoolBinds(binds)
			return nil, nil, fmt.Errorf("open isolation mount %q/%q: %w", selector.Root, selector.SubPath, err)
		}
		binds = append(binds, isolation.BindMount{
			SourceFile: os.NewFile(uintptr(fd), selector.Root+":"+selector.SubPath),
			Dest:       selector.Target,
			ReadOnly:   selector.Mode == "ro",
		})
	}
	masks := make([]string, 0, len(maskSet))
	for mask := range maskSet {
		masks = append(masks, mask)
	}
	sort.Strings(masks)
	return binds, masks, nil
}

func closePoolBinds(binds []isolation.BindMount) {
	for _, bind := range binds {
		if bind.SourceFile != nil {
			_ = bind.SourceFile.Close()
		}
	}
}

func pinPoolNamespaces(pid int) ([]*os.File, error) {
	names := []string{"user", "mnt", "ipc", "uts", "cgroup", "pid"}
	files := make([]*os.File, 0, len(names))
	for _, name := range names {
		fd, err := unix.Open(fmt.Sprintf("/proc/%d/ns/%s", pid, name), unix.O_RDONLY|unix.O_CLOEXEC, 0)
		if err != nil {
			closePoolFiles(files)
			return nil, fmt.Errorf("pin pool bwrap %s namespace: %w", name, err)
		}
		files = append(files, os.NewFile(uintptr(fd), "pool-bwrap-ns-"+name))
	}
	return files, nil
}

func closePoolFiles(files []*os.File) {
	for _, file := range files {
		_ = file.Close()
	}
}

func findPoolNsenter() (string, error) {
	for _, candidate := range []string{
		"/opt/opensandbox/opensandbox-nsenter",
		"/usr/local/libexec/opensandbox-nsenter",
	} {
		if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() && info.Mode()&0111 != 0 {
			return candidate, nil
		}
	}
	return "", errors.New("opensandbox-nsenter helper is unavailable")
}

func monitorPoolRuntime(state *poolRuntimeState) {
	_ = state.process.Wait()
	state.valid.Store(false)
	activePoolRuntime.CompareAndSwap(state, nil)
	_ = state.root.Close()
	closePoolFiles(state.namespaces)
	log.Error("pool bwrap runtime exited; all user operations are now denied")
}

// Close terminates the runtime and invalidates all future operations.
func (m *PoolRuntimeManager) Close() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.state == nil {
		return nil
	}
	m.state.valid.Store(false)
	activePoolRuntime.CompareAndSwap(m.state, nil)
	if m.state.process != nil && m.state.process.cmd.Process != nil {
		_ = syscall.Kill(-m.state.process.cmd.Process.Pid, syscall.SIGKILL)
	}
	m.state.lifecycle.Abort()
	_ = m.state.root.Close()
	closePoolFiles(m.state.namespaces)
	return m.state.lifecycle.Close()
}

// PoolRuntimeHealthy reports whether a configured runtime is still valid.
func PoolRuntimeHealthy() bool {
	state := activePoolRuntime.Load()
	return state != nil && state.valid.Load()
}

// MapFilesystemPath resolves an absolute guest path through the pinned bwrap
// root. The descriptor is CLOEXEC and is never inherited by user processes.
func MapFilesystemPath(guestPath string) (string, error) {
	state := activePoolRuntime.Load()
	if state == nil {
		if poolRuntimeRequired.Load() {
			return "", errors.New("pool bwrap runtime is unavailable")
		}
		return guestPath, nil
	}
	if !state.valid.Load() {
		return "", errors.New("pool bwrap runtime is unavailable")
	}
	rootPrefix := filepath.Join("/proc/self/fd", strconv.Itoa(int(state.root.Fd())))
	if guestPath == rootPrefix || strings.HasPrefix(guestPath, rootPrefix+string(os.PathSeparator)) {
		return guestPath, nil
	}
	if !filepath.IsAbs(guestPath) || filepath.Clean(guestPath) != guestPath {
		return "", fmt.Errorf("pool runtime path must be normalized and absolute: %q", guestPath)
	}
	// Reject symlinks in the current path before presenting it to legacy file
	// handlers through the pinned root descriptor. For create operations, walk
	// up to the nearest existing parent. The root FD keeps resolution inside
	// the bwrap mount tree even when the container root is otherwise writable.
	relative := strings.TrimPrefix(guestPath, "/")
	probe := relative
	for {
		fd, err := unix.Openat2(int(state.root.Fd()), probe, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_CLOEXEC | unix.O_NOFOLLOW,
			Resolve: unix.RESOLVE_IN_ROOT | unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		if err == nil {
			_ = unix.Close(fd)
			break
		}
		if !errors.Is(err, unix.ENOENT) {
			return "", fmt.Errorf("resolve pool runtime path %q: %w", guestPath, err)
		}
		parent := filepath.Dir(probe)
		if parent == probe || parent == "." {
			break
		}
		probe = parent
	}
	return filepath.Join(rootPrefix, relative), nil
}

func wrapPoolCommand(cmd *exec.Cmd, bypass bool) error {
	if bypass {
		return nil
	}
	state := activePoolRuntime.Load()
	if state == nil {
		if poolRuntimeRequired.Load() {
			return errors.New("pool bwrap runtime is unavailable")
		}
		return nil
	}
	if !state.valid.Load() {
		return errors.New("pool bwrap runtime is unavailable")
	}
	cwd := cmd.Dir
	if cwd == "" {
		cwd = "-"
	}
	originalArgs := append([]string(nil), cmd.Args...)
	namespaceArgs := make([]string, 0, len(state.namespaces))
	for _, file := range state.namespaces {
		childFD := 3 + len(cmd.ExtraFiles)
		cmd.ExtraFiles = append(cmd.ExtraFiles, file)
		namespaceArgs = append(namespaceArgs, strconv.Itoa(childFD))
	}
	cmd.Path = state.helper
	cmd.Args = append([]string{state.helper}, namespaceArgs...)
	cmd.Args = append(cmd.Args, cwd, "--")
	cmd.Args = append(cmd.Args, originalArgs...)
	cmd.Dir = ""
	return nil
}
