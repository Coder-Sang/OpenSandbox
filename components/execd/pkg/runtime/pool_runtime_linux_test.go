//go:build linux

// Copyright 2026 Alibaba Group Holding Ltd.
// Licensed under the Apache License, Version 2.0

package runtime

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/alibaba/opensandbox/execd/pkg/isolation"
	"github.com/stretchr/testify/require"
	"golang.org/x/sys/unix"
)

func validPoolIsolationSpec() *PoolIsolationSpec {
	return &PoolIsolationSpec{
		Type: "bwrap",
		Roots: map[string]PoolMountRoot{
			"projects": {
				MountRoot:      "/storage",
				Source:         "/storage/projects",
				TargetPrefixes: []string{"/workspace"},
				MaxMode:        "rw",
			},
		},
		Mounts: []PoolMountSelector{{
			Root: "projects", SubPath: "project-A", Target: "/workspace/a", Mode: "rw",
		}},
	}
}

func TestOpenPoolJitiCache(t *testing.T) {
	cache := filepath.Join(t.TempDir(), "jiti")
	require.NoError(t, os.Mkdir(cache, 0o700))
	bind, err := openPoolJitiCache(cache)
	require.NoError(t, err)
	t.Cleanup(func() { _ = bind.SourceFile.Close() })
	require.Equal(t, "/tmp/jiti", bind.Dest)
	require.False(t, bind.ReadOnly)
	require.NotNil(t, bind.SourceFile)

	// The source is pinned even if its pathname changes before bwrap mounts it.
	require.NoError(t, os.Rename(cache, cache+"-renamed"))
	_, err = bind.SourceFile.Stat()
	require.NoError(t, err)
}

func TestOpenPoolJitiCacheRejectsMissingOrUnsafeSource(t *testing.T) {
	dir := t.TempDir()
	_, err := openPoolJitiCache(filepath.Join(dir, "missing"))
	require.Error(t, err)

	file := filepath.Join(dir, "file")
	require.NoError(t, os.WriteFile(file, nil, 0o600))
	_, err = openPoolJitiCache(file)
	require.Error(t, err)

	symlink := filepath.Join(dir, "symlink")
	require.NoError(t, os.Symlink(dir, symlink))
	_, err = openPoolJitiCache(symlink)
	require.Error(t, err)
}

func TestValidatePoolIsolation(t *testing.T) {
	require.NoError(t, ValidatePoolIsolation(validPoolIsolationSpec()))
}

func TestValidatePoolIsolationRejectsTraversal(t *testing.T) {
	spec := validPoolIsolationSpec()
	spec.Mounts[0].SubPath = "../sibling"
	require.ErrorContains(t, ValidatePoolIsolation(spec), "subPath")
}

func TestValidatePoolIsolationRejectsPermissionEscalation(t *testing.T) {
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MaxMode = "ro"
	spec.Roots["projects"] = root
	require.ErrorContains(t, ValidatePoolIsolation(spec), "maxMode")
}

func TestValidatePoolIsolationAllowsNestedTargets(t *testing.T) {
	spec := validPoolIsolationSpec()
	spec.Mounts = append(spec.Mounts, PoolMountSelector{
		Root: "projects", SubPath: "project-B", Target: "/workspace/a/nested", Mode: "ro",
	})
	require.NoError(t, ValidatePoolIsolation(spec))
}

func TestValidatePoolIsolationRejectsDuplicateTargets(t *testing.T) {
	spec := validPoolIsolationSpec()
	spec.Mounts = append(spec.Mounts, PoolMountSelector{
		Root: "projects", SubPath: "project-B", Target: "/workspace/a", Mode: "ro",
	})
	require.ErrorContains(t, ValidatePoolIsolation(spec), "duplicated")
}

func TestSortedPoolMountsUsesParentFirstCanonicalOrder(t *testing.T) {
	mounts := []PoolMountSelector{
		{Target: "/workspace/z/child"},
		{Target: "/workspace/z"},
		{Target: "/workspace/a"},
		{Target: "/workspace"},
	}

	sorted := sortedPoolMounts(mounts)

	require.Equal(t, []string{
		"/workspace",
		"/workspace/a",
		"/workspace/z",
		"/workspace/z/child",
	}, []string{sorted[0].Target, sorted[1].Target, sorted[2].Target, sorted[3].Target})
	require.Equal(t, "/workspace/z/child", mounts[0].Target, "input order must not be mutated")
}

func TestNearestPoolMountAncestorUsesClosestPath(t *testing.T) {
	byTarget := map[string]int{
		"/workspace":         0,
		"/workspace/a":       1,
		"/workspace/a/deep":  2,
		"/workspace/another": 3,
	}
	require.Equal(t, 2, nearestPoolMountAncestor("/workspace/a/deep/child", byTarget))
	require.Equal(t, 0, nearestPoolMountAncestor("/workspace/new", byTarget))
	require.Equal(t, -1, nearestPoolMountAncestor("/other/new", byTarget))
}

func TestNearestPoolMountAncestorMatchesPreviousScan(t *testing.T) {
	targets := []string{
		"/projects/a", "/projects/a/src", "/projects/a/src/pkg",
		"/projects/a2", "/users/alice", "/users/alice/cache",
		"/tmp/client-gateway/git-metadata/a", "/opt/client-gateway/runtime-packages/a",
	}
	byTarget := make(map[string]int, len(targets))
	for index, target := range targets {
		byTarget[target] = index
	}
	for _, target := range append(targets, "/projects/a/src/pkg/deep", "/projects/missing", "/unrelated") {
		previous := -1
		previousDepth := -1
		for index, candidate := range targets {
			if candidate == target || !pathWithin(candidate, target) {
				continue
			}
			depth := strings.Count(candidate, "/")
			if depth > previousDepth {
				previous = index
				previousDepth = depth
			}
		}
		require.Equal(t, previous, nearestPoolMountAncestor(target, byTarget), target)
	}
}

func TestPoolDurationPercentileNearestRank(t *testing.T) {
	values := []time.Duration{time.Millisecond, 2 * time.Millisecond, 3 * time.Millisecond, 4 * time.Millisecond}
	require.Equal(t, 2*time.Millisecond, poolDurationPercentile(values, 50))
	require.Equal(t, 4*time.Millisecond, poolDurationPercentile(values, 95))
	require.Zero(t, poolDurationPercentile(nil, 95))
}

func TestValidatePoolIsolationRejectsControlTarget(t *testing.T) {
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.TargetPrefixes = []string{"/opt"}
	spec.Roots["projects"] = root
	spec.Mounts[0].Target = "/opt/opensandbox"
	require.ErrorContains(t, ValidatePoolIsolation(spec), "control path")
}

func TestValidatePoolIsolationRejectsControlMountRoot(t *testing.T) {
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MountRoot = "/opt"
	root.Source = "/opt/projects"
	spec.Roots["projects"] = root
	require.ErrorContains(t, ValidatePoolIsolation(spec), "mountRoot overlaps control path")
}

func TestPreferredPoolUIDMode(t *testing.T) {
	tests := []struct {
		name  string
		probe isolation.ProbeResult
		want  isolation.UidMode
	}{
		{name: "prefer userns", probe: isolation.ProbeResult{UsernsAvailable: true, SetprivAvailable: true}, want: isolation.UidModeUserns},
		{name: "fallback setpriv", probe: isolation.ProbeResult{SetprivAvailable: true}, want: isolation.UidModeSetpriv},
		{name: "neither", probe: isolation.ProbeResult{}, want: ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			require.Equal(t, tt.want, preferredPoolUIDMode(tt.probe))
		})
	}
}

func TestRequirePoolVarRunAlias(t *testing.T) {
	root := t.TempDir()
	runPath := filepath.Join(root, "run")
	varRunPath := filepath.Join(root, "var-run")
	require.NoError(t, os.Mkdir(runPath, 0o755))
	require.NoError(t, os.Symlink(runPath, varRunPath))
	require.NoError(t, requirePoolVarRunAlias(runPath, varRunPath))

	require.NoError(t, os.Remove(varRunPath))
	require.NoError(t, os.Mkdir(varRunPath, 0o755))
	require.ErrorContains(t, requirePoolVarRunAlias(runPath, varRunPath), "must be a symlink")

	require.NoError(t, os.Remove(varRunPath))
	otherPath := filepath.Join(root, "other")
	require.NoError(t, os.Mkdir(otherPath, 0o755))
	require.NoError(t, os.Symlink(otherPath, varRunPath))
	require.ErrorContains(t, requirePoolVarRunAlias(runPath, varRunPath), "instead of /run")
}

func TestPoolRuntimeMaskPathsUsePrivateRun(t *testing.T) {
	storageMasks := []string{"/mnt/storage"}
	masks := poolRuntimeMaskPaths(storageMasks)

	require.Equal(t, []string{"/mnt/storage", "/opt/opensandbox", "/run/execd"}, masks)
	require.Equal(t, []string{"/mnt/storage"}, storageMasks)
	require.NotContains(t, masks, "/var/run/secrets/kubernetes.io/serviceaccount")
}

func TestPoolRuntimeExitSignalsFatalOnceAndCleansPins(t *testing.T) {
	manager := NewPoolRuntimeManager(nil, isolation.ProbeResult{}, false)
	root, err := os.CreateTemp(t.TempDir(), "root-pin")
	require.NoError(t, err)
	namespace, err := os.CreateTemp(t.TempDir(), "namespace-pin")
	require.NoError(t, err)
	state := &poolRuntimeState{
		root:       root,
		namespaces: []*os.File{namespace},
	}
	state.valid.Store(true)
	activePoolRuntime.Store(state)
	t.Cleanup(func() { activePoolRuntime.Store(nil) })

	manager.handleRuntimeExit(state, nil)

	require.False(t, state.valid.Load())
	require.Nil(t, activePoolRuntime.Load())
	_, rootErr := root.Stat()
	require.Error(t, rootErr)
	_, namespaceErr := namespace.Stat()
	require.Error(t, namespaceErr)
	select {
	case fatalErr := <-manager.Fatal():
		require.ErrorContains(t, fatalErr, "pool bwrap runtime exited unexpectedly")
	case <-time.After(time.Second):
		t.Fatal("manager did not report the runtime exit")
	}

	manager.handleRuntimeExit(state, fmt.Errorf("second observation"))
	select {
	case fatalErr := <-manager.Fatal():
		t.Fatalf("manager reported a second fatal error: %v", fatalErr)
	default:
	}
}

func TestPoolRuntimeExitDuringCloseDoesNotSignalFatal(t *testing.T) {
	manager := NewPoolRuntimeManager(nil, isolation.ProbeResult{}, false)
	require.NoError(t, manager.Close())
	state := &poolRuntimeState{}
	state.valid.Store(true)

	manager.handleRuntimeExit(state, fmt.Errorf("killed during shutdown"))

	require.False(t, state.valid.Load())
	select {
	case fatalErr := <-manager.Fatal():
		t.Fatalf("normal shutdown reported a fatal error: %v", fatalErr)
	default:
	}
}

func TestOpenPoolMountsPinsDirectoryAcrossRename(t *testing.T) {
	mountRoot := t.TempDir()
	projects := filepath.Join(mountRoot, "projects")
	selected := filepath.Join(projects, "project-A")
	require.NoError(t, os.MkdirAll(selected, 0o755))
	require.NoError(t, os.WriteFile(filepath.Join(selected, "identity.txt"), []byte("A"), 0o644))

	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MountRoot = mountRoot
	root.Source = projects
	spec.Roots["projects"] = root

	binds, _, _, err := openPoolMounts(spec)
	require.NoError(t, err)
	t.Cleanup(func() { closePoolBinds(binds) })

	require.NoError(t, os.Rename(selected, filepath.Join(projects, "project-A-moved")))
	require.NoError(t, os.Mkdir(selected, 0o755))
	require.NoError(t, os.WriteFile(filepath.Join(selected, "identity.txt"), []byte("replacement"), 0o644))

	identity, err := os.ReadFile(fmt.Sprintf("/proc/self/fd/%d/identity.txt", binds[0].SourceFile.Fd()))
	require.NoError(t, err)
	require.Equal(t, "A", string(identity))
}

func TestOpenPoolMountsRejectsSymlinkRenameRace(t *testing.T) {
	originalWorkers := poolPinWorkerCount
	poolPinWorkerCount = 4
	t.Cleanup(func() { poolPinWorkerCount = originalWorkers })
	mountRoot := t.TempDir()
	projects := filepath.Join(mountRoot, "projects")
	selected := filepath.Join(projects, "project-A")
	real := filepath.Join(projects, "project-A-real")
	sibling := filepath.Join(projects, "project-B")
	require.NoError(t, os.MkdirAll(selected, 0o755))
	require.NoError(t, os.MkdirAll(sibling, 0o755))
	require.NoError(t, os.MkdirAll(filepath.Join(projects, "project-C"), 0o755))
	require.NoError(t, os.MkdirAll(filepath.Join(projects, "project-D"), 0o755))
	require.NoError(t, os.WriteFile(filepath.Join(selected, "identity.txt"), []byte("A"), 0o644))
	require.NoError(t, os.WriteFile(filepath.Join(sibling, "identity.txt"), []byte("B"), 0o644))

	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MountRoot = mountRoot
	root.Source = projects
	spec.Roots["projects"] = root
	spec.Mounts = append(spec.Mounts,
		PoolMountSelector{Root: "projects", SubPath: "project-B", Target: "/workspace/b", Mode: "rw"},
		PoolMountSelector{Root: "projects", SubPath: "project-C", Target: "/workspace/c", Mode: "rw"},
		PoolMountSelector{Root: "projects", SubPath: "project-D", Target: "/workspace/d", Mode: "rw"},
	)

	var stop atomic.Bool
	done := make(chan struct{})
	go func() {
		defer close(done)
		for !stop.Load() {
			if os.Rename(selected, real) != nil {
				continue
			}
			_ = os.Symlink("project-B", selected)
			_ = os.Remove(selected)
			_ = os.Rename(real, selected)
		}
	}()

	successes := 0
	for range 500 {
		binds, _, _, err := openPoolMounts(spec)
		if err != nil {
			continue
		}
		identity, readErr := os.ReadFile(fmt.Sprintf("/proc/self/fd/%d/identity.txt", binds[0].SourceFile.Fd()))
		closePoolBinds(binds)
		require.NoError(t, readErr)
		require.Equal(t, "A", string(identity), "openat2 followed a racing sibling symlink")
		successes++
	}
	stop.Store(true)
	<-done
	require.Positive(t, successes)
}

func TestOpenPoolMountsClosesPinnedRootAndSourceAfterFailure(t *testing.T) {
	mountRoot := t.TempDir()
	projects := filepath.Join(mountRoot, "projects")
	require.NoError(t, os.MkdirAll(filepath.Join(projects, "present"), 0o755))
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MountRoot = mountRoot
	root.Source = projects
	spec.Roots["projects"] = root
	spec.Mounts = []PoolMountSelector{
		{Root: "projects", SubPath: "present", Target: "/workspace/a", Mode: "rw"},
		{Root: "projects", SubPath: "missing", Target: "/workspace/b", Mode: "rw"},
	}
	before, err := os.ReadDir("/proc/self/fd")
	require.NoError(t, err)
	_, _, _, err = openPoolMounts(spec)
	require.Error(t, err)
	after, err := os.ReadDir("/proc/self/fd")
	require.NoError(t, err)
	require.Len(t, after, len(before), "failed mount preparation leaked file descriptors")
}

func TestOpenPoolMountsConcurrentMatchesSerialAndCleansFailure(t *testing.T) {
	mountRoot := t.TempDir()
	projects := filepath.Join(mountRoot, "projects")
	require.NoError(t, os.MkdirAll(projects, 0o755))
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MountRoot = mountRoot
	root.Source = projects
	spec.Roots["projects"] = root
	spec.Mounts = make([]PoolMountSelector, 128)
	for index := range spec.Mounts {
		name := fmt.Sprintf("project-%03d", index)
		require.NoError(t, os.Mkdir(filepath.Join(projects, name), 0o755))
		mode := "rw"
		if index%2 == 1 {
			mode = "ro"
		}
		spec.Mounts[index] = PoolMountSelector{
			Root: "projects", SubPath: name,
			Target: "/workspace/" + name, Mode: mode,
		}
	}
	originalWorkers := poolPinWorkerCount
	t.Cleanup(func() { poolPinWorkerCount = originalWorkers })
	poolPinWorkerCount = 1
	baselineBinds, baselineExpected, _, err := openPoolMounts(spec)
	require.NoError(t, err)
	defer closePoolBinds(baselineBinds)
	for _, workerCount := range []int{4, 8, 16} {
		poolPinWorkerCount = workerCount
		timings := poolMountTimings{}
		binds, expected, _, openErr := openPoolMountsWithTimings(spec, &timings)
		require.NoError(t, openErr)
		require.Equal(t, baselineExpected, expected)
		require.Equal(t, workerCount, timings.workers)
		require.Equal(t, len(spec.Mounts), timings.openCount)
		require.Equal(t, len(spec.Mounts), timings.statCount)
		for index, bind := range binds {
			require.Equal(t, baselineBinds[index].Dest, bind.Dest)
			require.Equal(t, baselineBinds[index].ReadOnly, bind.ReadOnly)
		}
		closePoolBinds(binds)
	}
	spec.Mounts[120].SubPath = "missing"
	before, err := os.ReadDir("/proc/self/fd")
	require.NoError(t, err)
	for _, workerCount := range []int{4, 8, 16} {
		poolPinWorkerCount = workerCount
		_, _, _, openErr := openPoolMounts(spec)
		require.ErrorContains(t, openErr, "open isolation mount")
		after, readErr := os.ReadDir("/proc/self/fd")
		require.NoError(t, readErr)
		require.Len(t, after, len(before), "failed concurrent pin leaked file descriptors")
	}
}

func BenchmarkPrepareNestedPoolMountpointsUnrelated(b *testing.B) {
	mounts := make([]PoolMountSelector, 1669)
	for i := range mounts {
		mounts[i].Target = fmt.Sprintf("/projects/%d", i)
	}
	binds := make([]isolation.BindMount, len(mounts))
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if err := prepareNestedPoolMountpoints(mounts, binds); err != nil {
			b.Fatal(err)
		}
	}
}

func BenchmarkPreviousNestedAncestorScanUnrelated(b *testing.B) {
	mounts := make([]PoolMountSelector, 1669)
	for i := range mounts {
		mounts[i].Target = fmt.Sprintf("/projects/%d", i)
	}
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		for childIndex, child := range mounts {
			nearestDepth := -1
			for candidateIndex, candidate := range mounts {
				if candidateIndex == childIndex || candidate.Target == child.Target || !pathWithin(candidate.Target, child.Target) {
					continue
				}
				depth := strings.Count(candidate.Target, "/")
				if depth > nearestDepth {
					nearestDepth = depth
				}
			}
		}
	}
}

func TestOpenPoolMountsCreatesMissingNestedTargetThroughWritableParent(t *testing.T) {
	spec, parentSource := nestedPoolIsolationSpec(t, "rw", false)

	binds, expectations, _, err := openPoolMounts(spec)
	require.NoError(t, err)
	t.Cleanup(func() { closePoolBinds(binds) })

	require.Equal(t, []string{"/workspace", "/workspace/cache/content"}, []string{binds[0].Dest, binds[1].Dest})
	require.Equal(t, []string{"/workspace", "/workspace/cache/content"}, []string{expectations[0].target, expectations[1].target})
	info, err := os.Stat(filepath.Join(parentSource, "cache", "content"))
	require.NoError(t, err)
	require.True(t, info.IsDir())
}

func TestOpenPoolMountsRejectsMissingNestedTargetThroughReadOnlyParent(t *testing.T) {
	spec, parentSource := nestedPoolIsolationSpec(t, "ro", false)

	_, _, _, err := openPoolMounts(spec)

	require.ErrorContains(t, err, "does not exist beneath read-only parent")
	require.NoDirExists(t, filepath.Join(parentSource, "cache"))
}

func TestOpenPoolMountsAllowsWritableChildThroughReadOnlyParent(t *testing.T) {
	spec, _ := nestedPoolIsolationSpec(t, "ro", true)

	binds, _, _, err := openPoolMounts(spec)
	require.NoError(t, err)
	t.Cleanup(func() { closePoolBinds(binds) })
	require.True(t, binds[0].ReadOnly)
	require.False(t, binds[1].ReadOnly)
}

func TestOpenPoolMountsConcurrentNestedTargets(t *testing.T) {
	originalWorkers := poolPinWorkerCount
	poolPinWorkerCount = 4
	t.Cleanup(func() { poolPinWorkerCount = originalWorkers })
	for _, test := range []struct {
		name         string
		parentMode   string
		createTarget bool
		wantError    string
	}{
		{name: "writable parent creates target", parentMode: "rw"},
		{name: "read only parent rejects missing target", parentMode: "ro", wantError: "does not exist beneath read-only parent"},
		{name: "read only parent allows existing target", parentMode: "ro", createTarget: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			spec, _ := nestedPoolIsolationSpec(t, test.parentMode, test.createTarget)
			binds, _, _, err := openPoolMounts(spec)
			if test.wantError != "" {
				require.ErrorContains(t, err, test.wantError)
				return
			}
			require.NoError(t, err)
			t.Cleanup(func() { closePoolBinds(binds) })
			require.Equal(t, "/workspace", binds[0].Dest)
			require.Equal(t, "/workspace/cache/content", binds[1].Dest)
		})
	}
}

func TestOpenPoolMountsRejectsNestedSymlinkTarget(t *testing.T) {
	spec, parentSource := nestedPoolIsolationSpec(t, "rw", false)
	require.NoError(t, os.Symlink("../child-source", filepath.Join(parentSource, "cache")))

	_, _, _, err := openPoolMounts(spec)

	require.Error(t, err)
	require.NotErrorIs(t, err, os.ErrNotExist)
}

func TestMkdirAllPoolDirectoryAtDoesNotFollowRacingSiblingSymlink(t *testing.T) {
	parent := t.TempDir()
	selected := filepath.Join(parent, "cache")
	real := filepath.Join(parent, "cache-real")
	sibling := filepath.Join(parent, "sibling")
	require.NoError(t, os.Mkdir(selected, 0o755))
	require.NoError(t, os.Mkdir(sibling, 0o755))
	parentFD, err := unix.Open(parent, unix.O_PATH|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	require.NoError(t, err)
	t.Cleanup(func() { _ = unix.Close(parentFD) })

	var stop atomic.Bool
	done := make(chan struct{})
	go func() {
		defer close(done)
		for !stop.Load() {
			if os.Rename(selected, real) != nil {
				continue
			}
			_ = os.Symlink("sibling", selected)
			_ = os.Remove(selected)
			_ = os.Rename(real, selected)
		}
	}()
	t.Cleanup(func() {
		stop.Store(true)
		<-done
	})

	for range 250 {
		err := mkdirAllPoolDirectoryAt(parentFD, filepath.Join("cache", "content"), "/workspace/cache/content")
		if err != nil {
			continue
		}
		require.NoDirExists(t, filepath.Join(sibling, "content"), "mkdirat escaped through a racing symlink")
	}
	require.NoDirExists(t, filepath.Join(sibling, "content"), "mkdirat escaped through a racing symlink")
}

func TestParsePoolMountInfo(t *testing.T) {
	contents := "36 25 0:32 / /workspace\\040root ro,nosuid,nodev - ext4 /dev/root rw\n" +
		"37 36 0:33 / /workspace\\040root/cache rw,nosuid - tmpfs tmpfs rw\n"

	mounts, err := parsePoolMountInfo(contents)

	require.NoError(t, err)
	require.Equal(t, poolMountInfo{target: "/workspace root", mode: "ro"}, mounts[36])
	require.Equal(t, poolMountInfo{target: "/workspace root/cache", mode: "rw"}, mounts[37])
}

func TestVerifyPoolMountsChecksIdentityAndMode(t *testing.T) {
	rootFD, err := unix.Open("/", unix.O_PATH|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	require.NoError(t, err)
	root := os.NewFile(uintptr(rootFD), "root")
	t.Cleanup(func() { _ = root.Close() })
	mountFD, err := unix.Open("/proc", unix.O_PATH|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	require.NoError(t, err)
	defer func() { _ = unix.Close(mountFD) }()
	var stat unix.Stat_t
	require.NoError(t, unix.Fstat(mountFD, &stat))
	mountInfo, err := readPoolMountInfo(os.Getpid())
	require.NoError(t, err)
	mode := ""
	for _, mount := range mountInfo {
		if mount.target == "/proc" {
			mode = mount.mode
			break
		}
	}
	require.NotEmpty(t, mode)
	expected := poolMountExpectation{target: "/proc", mode: mode, dev: uint64(stat.Dev), ino: stat.Ino}
	require.NoError(t, verifyPoolMounts(os.Getpid(), root, []poolMountExpectation{expected}))
	expected.ino++
	require.ErrorContains(t, verifyPoolMounts(os.Getpid(), root, []poolMountExpectation{expected}), "identity mismatch")
	expected.ino--
	if mode == "ro" {
		expected.mode = "rw"
	} else {
		expected.mode = "ro"
	}
	require.ErrorContains(t, verifyPoolMounts(os.Getpid(), root, []poolMountExpectation{expected}), "mode is")
}

func nestedPoolIsolationSpec(t *testing.T, parentMode string, createTarget bool) (*PoolIsolationSpec, string) {
	t.Helper()
	mountRoot := t.TempDir()
	projects := filepath.Join(mountRoot, "projects")
	parentSource := filepath.Join(projects, "parent-source")
	childSource := filepath.Join(projects, "child-source")
	require.NoError(t, os.MkdirAll(parentSource, 0o755))
	require.NoError(t, os.MkdirAll(childSource, 0o755))
	if createTarget {
		require.NoError(t, os.MkdirAll(filepath.Join(parentSource, "cache", "content"), 0o755))
	}
	return &PoolIsolationSpec{
		Type: "bwrap",
		Roots: map[string]PoolMountRoot{
			"projects": {
				MountRoot:      mountRoot,
				Source:         projects,
				TargetPrefixes: []string{"/workspace"},
				MaxMode:        "rw",
			},
		},
		Mounts: []PoolMountSelector{
			{Root: "projects", SubPath: "child-source", Target: "/workspace/cache/content", Mode: "rw"},
			{Root: "projects", SubPath: "parent-source", Target: "/workspace", Mode: parentMode},
		},
	}, parentSource
}
