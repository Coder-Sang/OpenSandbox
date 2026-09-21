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

const poolAnchorEnv = "OPENSANDBOX_POOL_ANCHOR=1"

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

type poolMountExpectation struct {
	target string
	mode   string
	dev    uint64
	ino    uint64
}

type poolMountInfo struct {
	target string
	mode   string
}

type poolRuntimeState struct {
	pid        int
	uidMode    isolation.UidMode
	root       *os.File
	namespaces []*os.File
	helper     string
	process    *managedProcess
	lifecycle  isolation.WorkloadLifecycle
	valid      atomic.Bool
	cleanup    sync.Once
}

var activePoolRuntime atomic.Pointer[poolRuntimeState]
var poolRuntimeRequired atomic.Bool

// PoolRuntimeManager owns the single long-lived bwrap runtime for a pooled
// Pod. It is intentionally one-shot: a failed or exited runtime is never
// replaced in the same Pod.
type PoolRuntimeManager struct {
	isolator  isolation.LifecycleIsolator
	uidMode   isolation.UidMode
	mu        sync.Mutex
	started   bool
	state     *poolRuntimeState
	closing   atomic.Bool
	fatal     chan error
	fatalOnce sync.Once
}

// NewPoolRuntimeManager creates a manager. A nil isolator is retained so a
// forced bwrap request fails closed instead of falling back to direct exec.
func NewPoolRuntimeManager(iso isolation.LifecycleIsolator, probe isolation.ProbeResult) *PoolRuntimeManager {
	return &PoolRuntimeManager{
		isolator: iso,
		uidMode:  preferredPoolUIDMode(probe),
		fatal:    make(chan error, 1),
	}
}

// Fatal reports an unexpected exit of the one-shot Pool runtime. The channel
// never closes; at most one terminal error is delivered for a manager.
func (m *PoolRuntimeManager) Fatal() <-chan error {
	return m.fatal
}

// preferredPoolUIDMode chooses the strongest mode supported by the runtime.
// A blank result is intentionally retained so Start fails closed when neither
// mode passed the startup probe.
func preferredPoolUIDMode(probe isolation.ProbeResult) isolation.UidMode {
	if probe.UsernsAvailable {
		return isolation.UidModeUserns
	}
	if probe.SetprivAvailable {
		return isolation.UidModeSetpriv
	}
	return ""
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

	targets := make(map[string]struct{}, len(spec.Mounts))
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
		if _, exists := targets[mount.Target]; exists {
			return fmt.Errorf("isolation mount target %q is duplicated", mount.Target)
		}
		targets[mount.Target] = struct{}{}
	}
	return nil
}

func sortedPoolMounts(mounts []PoolMountSelector) []PoolMountSelector {
	sorted := append([]PoolMountSelector(nil), mounts...)
	sort.Slice(sorted, func(i, j int) bool {
		leftDepth := strings.Count(sorted[i].Target, "/")
		rightDepth := strings.Count(sorted[j].Target, "/")
		if leftDepth != rightDepth {
			return leftDepth < rightDepth
		}
		return sorted[i].Target < sorted[j].Target
	})
	return sorted
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
	if !m.uidMode.Valid() {
		return errors.New("pool bwrap runtime has no usable uid mode")
	}

	binds, expectations, masks, err := openPoolMounts(spec)
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
		Profile:               isolation.ProfileStrict,
		Binds:                 binds,
		ShareNet:              true,
		Uid:                   &zero,
		Gid:                   &zero,
		UidMode:               m.uidMode,
		RootWritable:          true,
		SkipWorkspace:         true,
		MaskPaths:             append(masks, "/opt/opensandbox", "/run/execd", "/var/run/secrets/kubernetes.io/serviceaccount"),
		DropCapabilities:      true,
		LifecycleControlStdin: true,
		EnvPassthrough:        isolation.EnvSpec{Mode: isolation.EnvModeDeny},
	}
	cmd := exec.Command("/bin/sh", "-c", "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done")
	cmd.Env = append(os.Environ(), poolAnchorEnv)
	// The anchor has no user-visible stderr. Route bwrap/gate diagnostics to
	// execd so a fail-closed startup identifies the failing boundary.
	cmd.Stderr = os.Stderr
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
	if err := verifyPoolMounts(identity.PID, root, expectations); err != nil {
		_ = root.Close()
		lifecycle.Abort()
		return fmt.Errorf("verify pool bwrap mounts: %w", err)
	}
	namespaceFiles, err := pinPoolNamespaces(identity.PID, m.uidMode)
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
	state := &poolRuntimeState{pid: identity.PID, uidMode: m.uidMode, root: root, namespaces: namespaceFiles, helper: helper, process: mp, lifecycle: lifecycle}
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
	go m.monitorPoolRuntime(state)
	log.Info("pool bwrap runtime ready pid=%d mounts=%d uid_mode=%s", identity.PID, len(spec.Mounts), m.uidMode)
	return nil
}

func openPoolMounts(spec *PoolIsolationSpec) ([]isolation.BindMount, []poolMountExpectation, []string, error) {
	mounts := sortedPoolMounts(spec.Mounts)
	binds := make([]isolation.BindMount, 0, len(mounts))
	expectations := make([]poolMountExpectation, 0, len(mounts))
	maskSet := make(map[string]struct{})
	for _, root := range spec.Roots {
		maskSet[root.MountRoot] = struct{}{}
	}
	// Open and pin every source before creating any nested destination. This
	// prevents a directory created for one selector from changing which source
	// a later selector resolves to.
	for _, selector := range mounts {
		root := spec.Roots[selector.Root]
		mountRootFD, err := unix.Openat2(unix.AT_FDCWD, root.MountRoot, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
			Resolve: unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		if err != nil {
			closePoolBinds(binds)
			return nil, nil, nil, fmt.Errorf("open isolation mountRoot %q: %w", selector.Root, err)
		}
		sourceRelative, err := filepath.Rel(root.MountRoot, root.Source)
		if err != nil {
			_ = unix.Close(mountRootFD)
			closePoolBinds(binds)
			return nil, nil, nil, fmt.Errorf("resolve isolation root %q: %w", selector.Root, err)
		}
		rootFD, err := unix.Openat2(mountRootFD, sourceRelative, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
			Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		_ = unix.Close(mountRootFD)
		if err != nil {
			closePoolBinds(binds)
			return nil, nil, nil, fmt.Errorf("open isolation root %q: %w", selector.Root, err)
		}
		fd, err := unix.Openat2(rootFD, selector.SubPath, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
			Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		_ = unix.Close(rootFD)
		if err != nil {
			closePoolBinds(binds)
			return nil, nil, nil, fmt.Errorf("open isolation mount %q/%q: %w", selector.Root, selector.SubPath, err)
		}
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); err != nil {
			_ = unix.Close(fd)
			closePoolBinds(binds)
			return nil, nil, nil, fmt.Errorf("stat isolation mount %q/%q: %w", selector.Root, selector.SubPath, err)
		}
		binds = append(binds, isolation.BindMount{
			SourceFile: os.NewFile(uintptr(fd), selector.Root+":"+selector.SubPath),
			Dest:       selector.Target,
			ReadOnly:   selector.Mode == "ro",
		})
		expectations = append(expectations, poolMountExpectation{
			target: selector.Target,
			mode:   selector.Mode,
			dev:    uint64(stat.Dev),
			ino:    stat.Ino,
		})
	}
	if err := prepareNestedPoolMountpoints(mounts, binds); err != nil {
		closePoolBinds(binds)
		return nil, nil, nil, err
	}
	masks := make([]string, 0, len(maskSet))
	for mask := range maskSet {
		masks = append(masks, mask)
	}
	sort.Strings(masks)
	return binds, expectations, masks, nil
}

func prepareNestedPoolMountpoints(mounts []PoolMountSelector, binds []isolation.BindMount) error {
	for childIndex, child := range mounts {
		parentIndex := nearestPoolMountAncestor(mounts, childIndex)
		if parentIndex < 0 {
			continue
		}
		parent := mounts[parentIndex]
		relative, err := filepath.Rel(parent.Target, child.Target)
		if err != nil || relative == "." || strings.HasPrefix(relative, ".."+string(os.PathSeparator)) {
			return fmt.Errorf("resolve nested mount target %q beneath %q", child.Target, parent.Target)
		}
		parentFD := int(binds[parentIndex].SourceFile.Fd())
		fd, openErr := openPoolDirectoryAt(parentFD, relative)
		if openErr == nil {
			_ = unix.Close(fd)
			continue
		}
		if !errors.Is(openErr, unix.ENOENT) {
			return fmt.Errorf("open nested mount target %q beneath %q: %w", child.Target, parent.Target, openErr)
		}
		if parent.Mode != "rw" {
			return fmt.Errorf("nested mount target %q does not exist beneath read-only parent %q", child.Target, parent.Target)
		}
		if err := mkdirAllPoolDirectoryAt(parentFD, relative, child.Target); err != nil {
			return fmt.Errorf("create nested mount target %q beneath %q: %w", child.Target, parent.Target, err)
		}
	}
	return nil
}

func nearestPoolMountAncestor(mounts []PoolMountSelector, childIndex int) int {
	child := mounts[childIndex]
	nearest := -1
	nearestDepth := -1
	for candidateIndex, candidate := range mounts {
		if candidateIndex == childIndex || candidate.Target == child.Target || !pathWithin(candidate.Target, child.Target) {
			continue
		}
		depth := strings.Count(candidate.Target, "/")
		if depth > nearestDepth {
			nearest = candidateIndex
			nearestDepth = depth
		}
	}
	return nearest
}

func openPoolDirectoryAt(parentFD int, relative string) (int, error) {
	return unix.Openat2(parentFD, relative, &unix.OpenHow{
		Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
		Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
	})
}

func mkdirAllPoolDirectoryAt(parentFD int, relative, guestTarget string) error {
	currentFD, err := unix.Dup(parentFD)
	if err != nil {
		return err
	}
	unix.CloseOnExec(currentFD)
	defer func() { _ = unix.Close(currentFD) }()

	parts := strings.Split(filepath.Clean(relative), string(os.PathSeparator))
	currentTarget := strings.TrimSuffix(guestTarget, "/"+relative)
	for _, part := range parts {
		if part == "" || part == "." || part == ".." {
			return fmt.Errorf("invalid nested target component %q", part)
		}
		currentTarget = filepath.Join(currentTarget, part)
		nextFD, openErr := openPoolDirectoryAt(currentFD, part)
		if errors.Is(openErr, unix.ENOENT) {
			if mkdirErr := unix.Mkdirat(currentFD, part, 0o755); mkdirErr != nil && !errors.Is(mkdirErr, unix.EEXIST) {
				return mkdirErr
			}
			nextFD, openErr = openPoolDirectoryAt(currentFD, part)
			if openErr == nil {
				log.Info("created nested Pool mount target %s", currentTarget)
			}
		}
		if openErr != nil {
			return openErr
		}
		_ = unix.Close(currentFD)
		currentFD = nextFD
	}
	return nil
}

func verifyPoolMounts(pid int, root *os.File, expectations []poolMountExpectation) error {
	mounts, err := readPoolMountInfo(pid)
	if err != nil {
		return err
	}
	for _, expected := range expectations {
		relative := strings.TrimPrefix(expected.target, "/")
		fd, err := unix.Openat2(int(root.Fd()), relative, &unix.OpenHow{
			Flags:   unix.O_PATH | unix.O_DIRECTORY | unix.O_CLOEXEC,
			Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS | unix.RESOLVE_NO_SYMLINKS,
		})
		if err != nil {
			return fmt.Errorf("open mounted target %q: %w", expected.target, err)
		}
		var stat unix.Stat_t
		statErr := unix.Fstat(fd, &stat)
		if statErr != nil {
			_ = unix.Close(fd)
			return fmt.Errorf("stat mounted target %q: %w", expected.target, statErr)
		}
		if uint64(stat.Dev) != expected.dev || stat.Ino != expected.ino {
			_ = unix.Close(fd)
			return fmt.Errorf(
				"mounted target %q identity mismatch: got dev=%d ino=%d, want dev=%d ino=%d",
				expected.target,
				uint64(stat.Dev),
				stat.Ino,
				expected.dev,
				expected.ino,
			)
		}
		var statx unix.Statx_t
		statxErr := unix.Statx(fd, "", unix.AT_EMPTY_PATH|unix.AT_SYMLINK_NOFOLLOW, unix.STATX_MNT_ID, &statx)
		_ = unix.Close(fd)
		if statxErr != nil {
			return fmt.Errorf("read mounted target %q mount ID: %w", expected.target, statxErr)
		}
		mount, exists := mounts[statx.Mnt_id]
		if !exists {
			return fmt.Errorf("mounted target %q mount ID %d is absent from mountinfo", expected.target, statx.Mnt_id)
		}
		if mount.target != expected.target {
			return fmt.Errorf("mounted target %q resolves to mountpoint %q", expected.target, mount.target)
		}
		if mount.mode != expected.mode {
			return fmt.Errorf("mounted target %q mode is %s, want %s", expected.target, mount.mode, expected.mode)
		}
	}
	return nil
}

func readPoolMountInfo(pid int) (map[uint64]poolMountInfo, error) {
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/mountinfo", pid))
	if err != nil {
		return nil, fmt.Errorf("read pool runtime mountinfo: %w", err)
	}
	return parsePoolMountInfo(string(data))
}

func parsePoolMountInfo(contents string) (map[uint64]poolMountInfo, error) {
	mounts := make(map[uint64]poolMountInfo)
	for lineNumber, line := range strings.Split(strings.TrimSpace(contents), "\n") {
		if line == "" {
			continue
		}
		fields := strings.Fields(line)
		if len(fields) < 7 {
			return nil, fmt.Errorf("mountinfo line %d has too few fields", lineNumber+1)
		}
		separator := -1
		for index := 6; index < len(fields); index++ {
			if fields[index] == "-" {
				separator = index
				break
			}
		}
		if separator < 0 || separator+3 >= len(fields) {
			return nil, fmt.Errorf("mountinfo line %d is malformed", lineNumber+1)
		}
		mountID, err := strconv.ParseUint(fields[0], 10, 64)
		if err != nil {
			return nil, fmt.Errorf("mountinfo line %d has invalid mount ID: %w", lineNumber+1, err)
		}
		mountpoint := unescapeMountInfoPath(fields[4])
		mode := ""
		for _, option := range strings.Split(fields[5], ",") {
			if option == "ro" || option == "rw" {
				mode = option
				break
			}
		}
		if mode == "" {
			return nil, fmt.Errorf("mountinfo line %d has no access mode", lineNumber+1)
		}
		mounts[mountID] = poolMountInfo{target: mountpoint, mode: mode}
	}
	return mounts, nil
}

func unescapeMountInfoPath(value string) string {
	replacer := strings.NewReplacer(
		`\040`, " ",
		`\011`, "\t",
		`\012`, "\n",
		`\134`, `\`,
	)
	return replacer.Replace(value)
}

func closePoolBinds(binds []isolation.BindMount) {
	for _, bind := range binds {
		if bind.SourceFile != nil {
			_ = bind.SourceFile.Close()
		}
	}
}

func pinPoolNamespaces(pid int, uidMode isolation.UidMode) ([]*os.File, error) {
	names := []string{"user", "mnt", "ipc", "uts", "cgroup", "pid"}
	files := make([]*os.File, 0, len(names))
	for _, name := range names {
		// setns(2) rejects re-entering the caller's current user namespace.
		// setpriv mode deliberately shares that namespace, so retain the fixed
		// helper argument slot but mark it as an explicit no-op.
		if name == "user" && uidMode == isolation.UidModeSetpriv {
			files = append(files, nil)
			continue
		}
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
		if file != nil {
			_ = file.Close()
		}
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

func (m *PoolRuntimeManager) monitorPoolRuntime(state *poolRuntimeState) {
	waitErr := state.process.Wait()
	m.handleRuntimeExit(state, waitErr)
}

func (m *PoolRuntimeManager) handleRuntimeExit(state *poolRuntimeState, waitErr error) {
	state.valid.Store(false)
	activePoolRuntime.CompareAndSwap(state, nil)
	cleanupPoolRuntimeState(state)

	if m.closing.Load() {
		log.Info("pool bwrap runtime exited during execd shutdown")
		return
	}

	exitCode := -1
	if state.process != nil {
		exitCode = state.process.ExitCode()
	}
	fatalErr := fmt.Errorf("pool bwrap runtime exited unexpectedly (exit_code=%d)", exitCode)
	if waitErr != nil {
		fatalErr = fmt.Errorf("%w: %v", fatalErr, waitErr)
	}
	log.Error("%v; terminating execd so the Pool Pod can be recycled", fatalErr)
	m.fatalOnce.Do(func() {
		m.fatal <- fatalErr
	})
}

func cleanupPoolRuntimeState(state *poolRuntimeState) {
	if state == nil {
		return
	}
	state.cleanup.Do(func() {
		if state.root != nil {
			_ = state.root.Close()
		}
		closePoolFiles(state.namespaces)
	})
}

// Close terminates the runtime and invalidates all future operations.
func (m *PoolRuntimeManager) Close() error {
	m.closing.Store(true)
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
	cleanupPoolRuntimeState(m.state)
	return m.state.lifecycle.Close()
}

// PoolRuntimeHealthy reports whether a configured runtime is still valid.
func PoolRuntimeHealthy() bool {
	state := activePoolRuntime.Load()
	return state != nil && state.valid.Load()
}

// PoolRuntimeUIDMode reports the mode selected for the active Pool runtime.
func PoolRuntimeUIDMode() string {
	state := activePoolRuntime.Load()
	if state == nil || !state.valid.Load() {
		return ""
	}
	return string(state.uidMode)
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
		if file == nil {
			namespaceArgs = append(namespaceArgs, "-")
			continue
		}
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
