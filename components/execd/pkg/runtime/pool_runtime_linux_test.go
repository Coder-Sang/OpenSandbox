//go:build linux

// Copyright 2026 Alibaba Group Holding Ltd.
// Licensed under the Apache License, Version 2.0

package runtime

import (
	"fmt"
	"os"
	"path/filepath"
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

func TestPoolRuntimeExitSignalsFatalOnceAndCleansPins(t *testing.T) {
	manager := NewPoolRuntimeManager(nil, isolation.ProbeResult{})
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
	manager := NewPoolRuntimeManager(nil, isolation.ProbeResult{})
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
	mountRoot := t.TempDir()
	projects := filepath.Join(mountRoot, "projects")
	selected := filepath.Join(projects, "project-A")
	real := filepath.Join(projects, "project-A-real")
	sibling := filepath.Join(projects, "project-B")
	require.NoError(t, os.MkdirAll(selected, 0o755))
	require.NoError(t, os.MkdirAll(sibling, 0o755))
	require.NoError(t, os.WriteFile(filepath.Join(selected, "identity.txt"), []byte("A"), 0o644))
	require.NoError(t, os.WriteFile(filepath.Join(sibling, "identity.txt"), []byte("B"), 0o644))

	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MountRoot = mountRoot
	root.Source = projects
	spec.Roots["projects"] = root

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
